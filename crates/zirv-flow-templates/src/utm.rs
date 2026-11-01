//! Automatic UTM parameters on email links (architecture 10.2 and 11.5 step 7).
//!
//! The pattern logic and the link-tagging step are pure. The caller expands the workspace's
//! patterns into [`UtmTags`] (the values come from the workspace settings, the flow, the step and
//! the template) and passes them in the render hooks. Nothing here reads a database, and no
//! pattern can name a person or event field: only the placeholders in [`PLACEHOLDERS`] exist.

use std::fmt;
use std::ops::Range;

use crate::links::{escape, find_ignore_case, hrefs, is_web_url, opt_outs, unescape};

/// The placeholders a pattern may use. Any other `{...}` is refused.
pub const PLACEHOLDERS: [&str; 7] = [
    "workspace",
    "flow",
    "flow_version",
    "step",
    "template",
    "template_version",
    "category",
];

/// The longest pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 200;

/// The longest domain list.
pub const MAX_DOMAINS: usize = 50;

/// The query keys the step adds, in the order it adds them.
const KEYS: [&str; 4] = ["utm_source", "utm_medium", "utm_campaign", "utm_content"];

/// Why a pattern is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// A `{name}` that is not one of [`PLACEHOLDERS`]; the name is as typed.
    UnknownPlaceholder(String),
    /// A `{` without a closing `}`, or a stray `}`.
    Unbalanced,
    /// The pattern is longer than [`MAX_PATTERN_BYTES`].
    TooLong,
    /// The pattern holds a control character.
    Control,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlaceholder(name) => write!(f, "unknown placeholder {{{name}}}"),
            Self::Unbalanced => f.write_str("unbalanced braces"),
            Self::TooLong => write!(f, "longer than {MAX_PATTERN_BYTES} bytes"),
            Self::Control => f.write_str("holds a control character"),
        }
    }
}

impl std::error::Error for PatternError {}

/// One piece of a parsed pattern.
enum Piece<'p> {
    Literal(&'p str),
    Placeholder(&'p str),
}

fn parse(pattern: &str) -> Result<Vec<Piece<'_>>, PatternError> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(PatternError::TooLong);
    }
    if pattern.chars().any(char::is_control) {
        return Err(PatternError::Control);
    }
    let mut pieces = Vec::new();
    let mut rest = pattern;
    while let Some(at) = rest.find(['{', '}']) {
        if rest.as_bytes()[at] == b'}' {
            return Err(PatternError::Unbalanced);
        }
        if at > 0 {
            pieces.push(Piece::Literal(&rest[..at]));
        }
        let after = &rest[at + 1..];
        let Some(close) = after.find('}') else {
            return Err(PatternError::Unbalanced);
        };
        let name = &after[..close];
        if !PLACEHOLDERS.contains(&name) {
            // A nested `{` (as in `{{ person.email }}`) names the text up to the first `}`.
            return Err(PatternError::UnknownPlaceholder(name.to_owned()));
        }
        pieces.push(Piece::Placeholder(name));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Literal(rest));
    }
    Ok(pieces)
}

/// Check a pattern: only known placeholders, balanced braces, a sane length.
pub fn validate_pattern(pattern: &str) -> Result<(), PatternError> {
    parse(pattern).map(|_| ())
}

/// The values the placeholders expand to. A value the caller does not have (`{flow}` in a preview
/// or a test send) is the empty string, and a parameter that expands to nothing is omitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct UtmValues<'a> {
    pub workspace: &'a str,
    pub flow: &'a str,
    pub flow_version: &'a str,
    pub step: &'a str,
    pub template: &'a str,
    pub template_version: &'a str,
    pub category: &'a str,
}

impl UtmValues<'_> {
    fn get(&self, name: &str) -> &str {
        match name {
            "workspace" => self.workspace,
            "flow" => self.flow,
            "flow_version" => self.flow_version,
            "step" => self.step,
            "template" => self.template,
            "template_version" => self.template_version,
            "category" => self.category,
            _ => "",
        }
    }
}

/// Lowercase `text` and join its alphanumeric runs with single hyphens.
pub fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

/// Percent-encode everything but the RFC 3986 unreserved characters.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Expand a pattern: each placeholder becomes its slugged value, then the whole result is
/// URL-encoded. A pattern that does not validate expands to nothing (patterns are validated when
/// saved).
pub fn expand(pattern: &str, values: &UtmValues<'_>) -> String {
    let Ok(pieces) = parse(pattern) else {
        return String::new();
    };
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Placeholder(name) => out.push_str(&slug(values.get(name))),
        }
    }
    url_encode(&out)
}

/// A domain as stored: trimmed, lower-case, no leading `*.` or `.`. `None` when it is not a
/// plausible host name.
pub fn normalize_domain(domain: &str) -> Option<String> {
    let lower = domain.trim().to_ascii_lowercase();
    let host = lower
        .strip_prefix("*.")
        .or_else(|| lower.strip_prefix('.'))
        .unwrap_or(&lower)
        .trim_end_matches('.');
    let labels_ok = host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    labels_ok.then(|| host.to_owned())
}

/// The already-expanded parameters and the domains that get them. An empty value omits its
/// parameter.
#[derive(Clone, Copy, Debug)]
pub struct UtmTags<'a> {
    pub source: &'a str,
    pub medium: &'a str,
    pub campaign: &'a str,
    pub content: &'a str,
    /// Normalised domains (see [`normalize_domain`]).
    pub domains: &'a [String],
}

impl UtmTags<'_> {
    fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            (KEYS[0], self.source),
            (KEYS[1], self.medium),
            (KEYS[2], self.campaign),
            (KEYS[3], self.content),
        ]
    }

    /// Whether `host` is a listed domain or a subdomain of one, on label boundaries.
    fn lists(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').as_bytes();
        self.domains.iter().any(|domain| {
            let domain = domain.as_bytes();
            let Some(split) = host.len().checked_sub(domain.len()) else {
                return false;
            };
            let (head, tail) = host.split_at(split);
            tail.eq_ignore_ascii_case(domain) && (head.is_empty() || head.ends_with(b"."))
        })
    }
}

/// The expanded parameters of one message, owned. A caller builds it from the workspace setting
/// and the message's context, and borrows [`UtmTags`] from it for the render hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtmPlan {
    source: String,
    medium: String,
    campaign: String,
    content: String,
    domains: Vec<String>,
}

impl UtmPlan {
    /// Expand the four patterns (`source`, `medium`, `campaign`, `content`) with `values`.
    pub fn new(patterns: [&str; 4], domains: &[String], values: &UtmValues<'_>) -> Self {
        Self {
            source: expand(patterns[0], values),
            medium: expand(patterns[1], values),
            campaign: expand(patterns[2], values),
            content: expand(patterns[3], values),
            domains: domains
                .iter()
                .filter_map(|domain| normalize_domain(domain))
                .collect(),
        }
    }

    /// The tags to put in [`crate::Hooks`].
    pub fn tags(&self) -> UtmTags<'_> {
        UtmTags {
            source: &self.source,
            medium: &self.medium,
            campaign: &self.campaign,
            content: &self.content,
            domains: &self.domains,
        }
    }

    /// The tagged form of `url`, or the URL as it is when it would not be tagged. For the live
    /// example in the settings screen and for tests.
    pub fn apply(&self, url: &str) -> String {
        tag_url(url, &self.tags()).unwrap_or_else(|| url.to_owned())
    }
}

/// The host of an `http` or `https` URL. `None` for an authority with a backslash: browsers read
/// a backslash as `/` in an http(s) URL, so the host is ambiguous and the URL is left alone.
fn host_of(url: &str) -> Option<&str> {
    let after_scheme = &url[url.find("://")? + 3..];
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('\\') {
        return None;
    }
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.starts_with('[') {
        return None;
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    (!host.is_empty()).then_some(host)
}

/// Add the UTM parameters to one URL. `None` when the URL is not tagged: not `http` or `https`,
/// an unlisted host, or nothing to add. Existing parameters are kept (never overwritten), the
/// new ones go after the existing query in the fixed order, and the fragment stays last.
pub fn tag_url(url: &str, utm: &UtmTags<'_>) -> Option<String> {
    if !is_web_url(url) || !host_of(url).is_some_and(|host| utm.lists(host)) {
        return None;
    }
    let (before_fragment, fragment) = match url.find('#') {
        Some(at) => (&url[..at], Some(&url[at..])),
        None => (url, None),
    };
    let (base, query) = match before_fragment.find('?') {
        Some(at) => (&before_fragment[..at], Some(&before_fragment[at + 1..])),
        None => (before_fragment, None),
    };
    // A key counts whether or not it is percent-encoded (`%75tm_source`), in any case.
    let present = |key: &str| {
        query.is_some_and(|q| {
            q.split('&').any(|pair| {
                let name = pair.split('=').next().unwrap_or_default();
                percent_decode(name).eq_ignore_ascii_case(key)
            })
        })
    };
    let added: Vec<String> = utm
        .pairs()
        .into_iter()
        .filter(|(key, value)| !value.is_empty() && !present(key))
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if added.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(url.len() + 64);
    out.push_str(base);
    out.push('?');
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        out.push_str(query);
        out.push('&');
    }
    out.push_str(&added.join("&"));
    if let Some(fragment) = fragment {
        out.push_str(fragment);
    }
    Some(out)
}

/// Decode the `%XX` escapes of a query key; a malformed escape stays as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let decoded = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tag the links of the HTML part with `utm` (when there is one) and remove the opt-out
/// attribute `data-zf-utm="off"` from every `<a>` tag, whether it has an `href` or not and
/// whether or not anything is tagged. Links in `keep` (the message's own unsubscribe and
/// preference URLs) are never tagged, and neither is a link whose tag carries the opt-out.
///
/// Returns the HTML and the URLs of the opted-out links, which the text part must not tag either.
/// HTML without the attribute and without `utm` comes back byte for byte.
pub(crate) fn process_html(
    html: &str,
    utm: Option<&UtmTags<'_>>,
    keep: &[&str],
) -> (String, Vec<String>) {
    if utm.is_none() && find_ignore_case(html, "data-zf-utm", 0).is_none() {
        return (html.to_owned(), Vec::new());
    }
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut skipped = Vec::new();
    let mut opted_out: Vec<Range<usize>> = Vec::new();
    for off in opt_outs(html) {
        edits.push((off.attribute, String::new()));
        if let Some(value) = off.href {
            skipped.push(unescape(html[value.clone()].trim()));
            opted_out.push(value);
        }
    }
    if let Some(utm) = utm {
        for href in hrefs(html) {
            if opted_out.contains(&href.value) {
                continue;
            }
            let url = unescape(html[href.value.clone()].trim());
            if keep.contains(&url.as_str()) {
                continue;
            }
            if let Some(tagged) = tag_url(&url, utm) {
                edits.push((href.outer.clone(), format!("\"{}\"", escape(&tagged))));
            }
        }
    }
    edits.sort_by_key(|(span, _)| span.start);
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    for (span, replacement) in edits {
        if span.start < copied {
            continue;
        }
        out.push_str(&html[copied..span.start]);
        out.push_str(&replacement);
        copied = span.end;
    }
    out.push_str(&html[copied..]);
    (out, skipped)
}

/// Tag the URLs of the text part the same way as the HTML part. A URL that starts inside another
/// one (`mailto:a@b?body=https://...`) is not a link of its own: it must start the text, follow
/// whitespace or one of `( < [ " '`. A URL equal to one in `skip` (the links the HTML part opted
/// out) is left alone, even when another HTML link to that URL is tagged: opt-out wins per URL.
pub(crate) fn tag_text(text: &str, utm: &UtmTags<'_>, keep: &[&str], skip: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(start) = next_url(text, at) {
        let stop = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
            .map_or(text.len(), |n| start + n);
        let url = text[start..stop].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
        out.push_str(&text[at..start]);
        if keep.contains(&url) || skip.iter().any(|skipped| skipped == url) {
            out.push_str(url);
        } else {
            out.push_str(&tag_url(url, utm).unwrap_or_else(|| url.to_owned()));
        }
        at = start + url.len();
    }
    out.push_str(&text[at..]);
    out
}

/// The position of the next `http://` or `https://` in `text` from `from`, ignoring ASCII case,
/// at a boundary: the start of the text, whitespace, or one of `( < [ " '` before it.
fn next_url(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    (from..bytes.len()).find(|&i| {
        // A match starts with an ASCII byte, so `i` is a character boundary for the look-behind.
        [&b"http://"[..], &b"https://"[..]].iter().any(|prefix| {
            bytes
                .get(i..i + prefix.len())
                .is_some_and(|b| b.eq_ignore_ascii_case(prefix))
        }) && text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '<' | '[' | '"' | '\''))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "utm_source=acme&utm_medium=email&utm_campaign=spring&utm_content=hero";

    fn domains() -> Vec<String> {
        vec!["example.com".to_owned()]
    }

    fn tags(domains: &[String]) -> UtmTags<'_> {
        UtmTags {
            source: "acme",
            medium: "email",
            campaign: "spring",
            content: "hero",
            domains,
        }
    }

    #[test]
    fn known_placeholders_validate() {
        for p in PLACEHOLDERS {
            assert_eq!(validate_pattern(&format!("a-{{{p}}}-b")), Ok(()));
        }
        assert_eq!(validate_pattern("plain"), Ok(()));
        assert_eq!(validate_pattern(""), Ok(()));
    }

    #[test]
    fn unknown_and_person_placeholders_are_refused_and_named() {
        assert_eq!(
            validate_pattern("{person.email}"),
            Err(PatternError::UnknownPlaceholder("person.email".into()))
        );
        assert_eq!(
            validate_pattern("x{nope}"),
            Err(PatternError::UnknownPlaceholder("nope".into()))
        );
        assert!(matches!(
            validate_pattern("{{ person.email }}"),
            Err(PatternError::UnknownPlaceholder(_))
        ));
        assert_eq!(validate_pattern("{flow"), Err(PatternError::Unbalanced));
        assert_eq!(validate_pattern("flow}"), Err(PatternError::Unbalanced));
        assert_eq!(
            validate_pattern(&"a".repeat(MAX_PATTERN_BYTES + 1)),
            Err(PatternError::TooLong)
        );
        assert_eq!(validate_pattern("a\nb"), Err(PatternError::Control));
    }

    #[test]
    fn expansion_slugs_values_and_encodes_the_result() {
        let values = UtmValues {
            workspace: "Acme Corp",
            flow: "Spring  Sale!",
            template: "Ünï Hero",
            ..UtmValues::default()
        };
        assert_eq!(expand("{workspace}", &values), "acme-corp");
        assert_eq!(expand("{flow}", &values), "spring-sale");
        assert_eq!(expand("{template}", &values), "%C3%BCn%C3%AF-hero");
        assert_eq!(expand("a b/{flow}", &values), "a%20b%2Fspring-sale");
        assert_eq!(expand("{step}", &values), "");
    }

    #[test]
    fn slug_lowercases_and_hyphenates() {
        assert_eq!(slug("  Hello, World_2 "), "hello-world-2");
        assert_eq!(slug("---"), "");
    }

    #[test]
    fn domains_normalise() {
        assert_eq!(
            normalize_domain(" *.Example.COM. "),
            Some("example.com".into())
        );
        assert_eq!(normalize_domain(".example.com"), Some("example.com".into()));
        for bad in [
            "",
            "localhost",
            "a b.com",
            "https://x.com",
            "-a.com",
            "a..com",
        ] {
            assert_eq!(normalize_domain(bad), None, "{bad}");
        }
    }

    #[test]
    fn listed_domain_and_subdomain_are_tagged_in_fixed_order() {
        let d = domains();
        let t = tags(&d);
        assert_eq!(
            tag_url("https://example.com/p", &t),
            Some(format!("https://example.com/p?{Q}"))
        );
        assert!(tag_url("http://shop.Example.com/p", &t).is_some());
        assert!(tag_url("https://a.b.example.com", &t).is_some());
    }

    #[test]
    fn unlisted_lookalike_and_other_schemes_are_not_tagged() {
        let d = domains();
        let t = tags(&d);
        for url in [
            "https://other.com/p",
            "https://evilexample.com/p",
            "https://example.com.evil.com/p",
            "https://evil.com/?u=example.com",
            "https://example.com@evil.com/p",
            "mailto:a@example.com",
            "tel:+4512345678",
            "#top",
            "ftp://example.com/x",
        ] {
            assert_eq!(tag_url(url, &t), None, "{url}");
        }
        assert!(tag_url("https://user@example.com:8443/p", &t).is_some());
    }

    #[test]
    fn existing_parameters_are_kept_and_never_overwritten() {
        let d = domains();
        let t = tags(&d);
        assert_eq!(
            tag_url("https://example.com/p?utm_campaign=mine&a=1", &t),
            Some(
                "https://example.com/p?utm_campaign=mine&a=1&utm_source=acme&utm_medium=email&utm_content=hero"
                    .to_owned()
            )
        );
        let all = "https://example.com/?utm_source=a&utm_medium=b&utm_campaign=c&utm_content=d";
        assert_eq!(tag_url(all, &t), None);
    }

    #[test]
    fn query_and_fragment_are_kept() {
        let d = domains();
        let t = tags(&d);
        assert_eq!(
            tag_url("https://example.com/p?a=1#sec", &t),
            Some(format!("https://example.com/p?a=1&{Q}#sec"))
        );
        assert_eq!(
            tag_url("https://example.com/#sec", &t),
            Some(format!("https://example.com/?{Q}#sec"))
        );
        assert_eq!(
            tag_url("https://example.com/?", &t),
            Some(format!("https://example.com/?{Q}"))
        );
    }

    #[test]
    fn an_empty_value_omits_its_parameter() {
        let d = domains();
        let t = UtmTags {
            campaign: "",
            ..tags(&d)
        };
        assert_eq!(
            tag_url("https://example.com/", &t).as_deref(),
            Some("https://example.com/?utm_source=acme&utm_medium=email&utm_content=hero")
        );
        let none = UtmTags {
            source: "",
            medium: "",
            campaign: "",
            content: "",
            domains: &d,
        };
        assert_eq!(tag_url("https://example.com/", &none), None);
    }

    #[test]
    fn html_links_are_tagged_and_others_left_alone() {
        let d = domains();
        let t = tags(&d);
        let html = concat!(
            r#"<a href="https://example.com/a?x=1&amp;y=2">a</a>"#,
            r#"<a href='https://other.com/b'>b</a>"#,
            r#"<a href="mailto:a@example.com">m</a>"#,
            r#"<a href="tel:123">t</a>"#,
            r##"<a href="#top">h</a>"##,
            r#"<a href="https://example.com/unsub?s=1&amp;t=2">u</a>"#,
            r#"<a href=https://shop.example.com/c>c</a>"#,
        );
        let out = process_html(html, Some(&t), &["https://example.com/unsub?s=1&t=2"]).0;
        let q = Q.replace('&', "&amp;");
        let expected = [
            format!(r#"<a href="https://example.com/a?x=1&amp;y=2&amp;{q}">a</a>"#),
            r#"<a href='https://other.com/b'>b</a>"#.to_owned(),
            r#"<a href="mailto:a@example.com">m</a>"#.to_owned(),
            r#"<a href="tel:123">t</a>"#.to_owned(),
            r##"<a href="#top">h</a>"##.to_owned(),
            r#"<a href="https://example.com/unsub?s=1&amp;t=2">u</a>"#.to_owned(),
            format!(r#"<a href="https://shop.example.com/c?{q}">c</a>"#),
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn opt_out_skips_the_link_and_is_stripped_wherever_it_sits() {
        let d = domains();
        let t = tags(&d);
        let html = concat!(
            r#"<a href="https://example.com/1" data-zf-utm="off">1</a>"#,
            r#"<a data-zf-utm='OFF' class="x" href="https://example.com/2">2</a>"#,
            r#"<a href="https://example.com/3" data-zf-utm=off>3</a>"#,
            r#"<a href="https://example.com/4" data-zf-utm="on">4</a>"#,
        );
        let out = process_html(html, Some(&t), &[]).0;
        assert!(out.starts_with(concat!(
            r#"<a href="https://example.com/1">1</a>"#,
            r#"<a class="x" href="https://example.com/2">2</a>"#,
            r#"<a href="https://example.com/3">3</a>"#,
        )));
        assert!(out.contains("example.com/4?utm_source=acme"));
        assert!(out.contains(r#"data-zf-utm="on""#));
        assert!(!out.contains(r#"data-zf-utm="off""#) && !out.contains("OFF"));
    }

    #[test]
    fn opt_out_wins_for_a_url_in_the_text_part_when_two_links_share_it() {
        let d = domains();
        let t = tags(&d);
        let html = concat!(
            r#"<a href="https://example.com/s" data-zf-utm="off">1</a>"#,
            r#"<a href="https://example.com/s">2</a>"#,
        );
        let (out, skipped) = process_html(html, Some(&t), &[]);
        assert_eq!(out.matches("utm_source=acme").count(), 1, "{out}");
        let text = tag_text("a https://example.com/s b", &t, &[], &skipped);
        assert_eq!(text, "a https://example.com/s b");
    }

    #[test]
    fn a_plan_expands_patterns_once_and_tags_like_the_tags_do() {
        let values = UtmValues {
            workspace: "Acme",
            template: "Hero",
            ..UtmValues::default()
        };
        let plan = UtmPlan::new(
            ["{workspace}", "email", "{flow}", "{template}"],
            &["Example.com".to_owned(), "nonsense".to_owned()],
            &values,
        );
        assert_eq!(
            plan.apply("https://example.com/p"),
            "https://example.com/p?utm_source=acme&utm_medium=email&utm_content=hero"
        );
        assert_eq!(plan.apply("https://other.com/p"), "https://other.com/p");
    }

    #[test]
    fn tagging_is_deterministic() {
        let d = domains();
        let t = tags(&d);
        let html = r#"<a href="https://example.com/a">a</a>"#;
        assert_eq!(
            process_html(html, Some(&t), &[]),
            process_html(html, Some(&t), &[])
        );
    }

    #[test]
    fn text_urls_get_the_same_parameters_as_html() {
        let d = domains();
        let t = tags(&d);
        let text = "See https://example.com/a. Or <https://shop.example.com/b?x=1#f>, not https://other.com/c or https://example.com/unsub.";
        let out = tag_text(text, &t, &["https://example.com/unsub"], &[]);
        assert_eq!(
            out,
            format!(
                "See https://example.com/a?{Q}. Or <https://shop.example.com/b?x=1&{Q}#f>, not https://other.com/c or https://example.com/unsub."
            )
        );
        assert_eq!(tag_text("no links", &t, &[], &[]), "no links");
    }

    #[test]
    fn a_percent_encoded_existing_utm_key_is_recognised_and_kept_as_written() {
        let d = domains();
        let t = tags(&d);
        let tagged = tag_url("https://example.com/?%75tm_source=old&%55TM_medium=m", &t).unwrap();
        assert_eq!(
            tagged,
            "https://example.com/?%75tm_source=old&%55TM_medium=m&utm_campaign=spring&utm_content=hero"
        );
    }

    #[test]
    fn a_backslash_in_the_authority_leaves_the_url_untagged() {
        let d = domains();
        let t = tags(&d);
        assert_eq!(tag_url(r"https://evil.test\@example.com/", &t), None);
        assert_eq!(tag_url(r"https://example.com\.evil.test/", &t), None);
        assert!(tag_url(r"https://example.com/ab", &t).is_some());
    }

    #[test]
    fn html_with_quoted_markup_comes_back_byte_identical_with_and_without_a_plan() {
        let d = domains();
        let t = tags(&d);
        let html = r#"<div title='<a data-zf-utm=off href=https://example.com/a>'>x</div><a href="https://other.test/b">b</a>"#;
        assert_eq!(process_html(html, None, &[]).0, html);
        assert_eq!(process_html(html, Some(&t), &[]).0, html);
    }

    #[test]
    fn a_text_url_after_unicode_whitespace_is_tagged() {
        let d = domains();
        let t = tags(&d);
        let out = tag_text(
            "see\u{00A0}https://example.com/a\u{3000}https://example.com/b",
            &t,
            &[],
            &[],
        );
        assert_eq!(
            out,
            format!("see\u{00A0}https://example.com/a?{Q}\u{3000}https://example.com/b?{Q}")
        );
    }
}
