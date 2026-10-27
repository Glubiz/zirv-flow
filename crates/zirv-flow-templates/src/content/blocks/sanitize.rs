//! The rich text sanitizer (architecture 11.2): an allowlist applied to the source at validation
//! and again to what MiniJinja rendered from it.
//!
//! `ammonia` does the cleaning. Its allowlist here is paragraphs, bold, italic, underline, links,
//! lists and line breaks; the only attribute is `href`, and it may be `https`, `http`, `mailto`
//! or start with a merge tag. Script and style elements are dropped with their content, comments
//! are dropped, and every other element is dropped with its markup but not its text.
//!
//! Merge tags are not HTML: `{{ a > 1 }}` would be rewritten, and `href="{{ u }}"` is not a URL.
//! So a source is first protected: every tag becomes a placeholder, the sanitizer reads the
//! rest, and the tags come back afterwards (see [`Protected`]).
//!
//! The render step cleans after MiniJinja too, because a merge value in a link can name a scheme
//! the allowlist refuses. MiniJinja's autoescape already keeps a value from adding markup, and
//! the source was clean before the render, so what is left to check is the shape of the result:
//! [`enforce`] reads its tokens and returns it unchanged when it holds nothing outside the
//! allowlist, so the bytes equal the converted MJML's, and only otherwise cleans it.

use std::borrow::Cow;
use std::sync::Arc;

use ammonia::{Builder, UrlRelative, UrlRelativeEvaluate};
use htmlparser::{ElementEnd, Token, Tokenizer};

use super::tags::{Protected, starts_with_tag};
use crate::links::unescape;

/// The elements rich text may hold.
pub(crate) const TAGS: [&str; 11] = [
    "p", "b", "strong", "i", "em", "u", "a", "ul", "ol", "li", "br",
];

/// The URL schemes a link may use besides a merge tag.
const SCHEMES: [&str; 3] = ["https", "http", "mailto"];

/// A cleaned rich text and its plain text form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Cleaned {
    pub(crate) html: String,
    pub(crate) text: String,
}

/// The ammonia configuration. `keep` is the placeholder test for a relative URL: a URL that
/// starts with a merge tag stays, any other relative URL goes.
fn builder(keep: impl Fn(&str) -> bool + Send + Sync + 'static) -> Builder<'static> {
    /// Keeps a relative URL when `keep` says so, and drops the attribute otherwise.
    struct Keep<K>(K);

    impl<K: Fn(&str) -> bool + Send + Sync + 'static> UrlRelativeEvaluate<'static> for Keep<K> {
        fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
            (self.0)(url).then_some(Cow::Borrowed(url))
        }
    }

    let mut builder = Builder::empty();
    builder
        .add_tags(TAGS)
        .generic_attributes(Default::default())
        .tag_attributes(Default::default())
        .add_tag_attributes("a", ["href"])
        .url_schemes(SCHEMES.into_iter().collect())
        .add_clean_content_tags(["script", "style"])
        .link_rel(None)
        .url_relative(UrlRelative::Custom(Box::new(Keep(keep))));
    builder
}

/// Clean rich text `source`, merge tags kept as they are, and give its plain text form (with the
/// tags kept too).
pub(crate) fn sanitize_source(source: &str) -> Cleaned {
    let protected = Arc::new(Protected::new(source));
    let probe = Arc::clone(&protected);
    let html = builder(move |url| probe.starts_with_placeholder(url))
        .clean(&protected.text)
        .to_string();
    let text = html_to_text(&html);
    Cleaned {
        html: protected.restore(&html),
        text: protected.restore(&text),
    }
}

/// Clean text that has no merge tags: relative URLs go.
pub(crate) fn clean_rendered(html: &str) -> String {
    builder(|_| false).clean(html).to_string()
}

/// What the allowlist would remove from `source`, as short descriptions (an element or attribute
/// name, or a URL scheme), in the order they appear. Empty when the source is clean.
pub(crate) fn violations(source: &str) -> Vec<String> {
    let protected = Protected::new(source);
    check(&protected.text, |url| {
        protected.starts_with_placeholder(url)
    })
}

/// Return `rendered` itself when it holds nothing outside the allowlist, and the cleaned text
/// when it does. The test reads tokens, and anything it cannot read counts as outside.
pub(crate) fn enforce(rendered: &str) -> String {
    if check(rendered, |_| false).is_empty() {
        rendered.to_owned()
    } else {
        clean_rendered(rendered)
    }
}

/// Whether a URL may be a link target; a URL that starts with a merge tag passes `merge`.
fn url_allowed(url: &str, merge: &impl Fn(&str) -> bool) -> bool {
    let url = unescape(url.trim());
    if merge(&url) {
        return true;
    }
    let lower = url.to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| {
        lower
            .strip_prefix(scheme)
            .is_some_and(|rest| rest.starts_with(':'))
    })
}

/// Run `read` over the tokens of the fragment `html`, or return `None` when it cannot be read.
/// The tokenizer reads one XML document, so the fragment is put in a root element of its own and
/// the root's three tokens are taken off again. Markup in the fragment that closes the root early
/// leaves a root token in the middle, which the allowlist refuses like any other element.
fn with_tokens<R>(html: &str, read: impl FnOnce(&[Token<'_>]) -> R) -> Option<R> {
    const ROOT: &str = "zqroot";
    let wrapped = format!("<{ROOT}>{html}</{ROOT}>");
    let tokens: Vec<Token<'_>> = Tokenizer::from(wrapped.as_str())
        .collect::<Result<_, _>>()
        .ok()?;
    match tokens.as_slice() {
        [
            Token::ElementStart { local: open, .. },
            Token::ElementEnd {
                end: ElementEnd::Open,
                ..
            },
            inner @ ..,
            Token::ElementEnd {
                end: ElementEnd::Close(_, close),
                ..
            },
        ] if open.as_str() == ROOT && close.as_str() == ROOT => Some(read(inner)),
        _ => None,
    }
}

/// The tokens of `html` that are not on the allowlist.
fn check(html: &str, merge: impl Fn(&str) -> bool) -> Vec<String> {
    with_tokens(html, |tokens| {
        let mut found = Vec::new();
        let mut element = String::new();
        for token in tokens {
            match token {
                Token::ElementStart { local, .. } => {
                    element = local.as_str().to_ascii_lowercase();
                    if !TAGS.contains(&element.as_str()) {
                        found.push(format!("<{element}>"));
                    }
                }
                Token::Attribute { local, value, .. } => {
                    let name = local.as_str().to_ascii_lowercase();
                    if element != "a" || name != "href" {
                        found.push(format!("{name}="));
                    } else if !value
                        .as_ref()
                        .is_some_and(|value| url_allowed(value.as_str(), &merge))
                    {
                        found.push("href=".to_owned());
                    }
                }
                Token::ElementEnd {
                    end: ElementEnd::Close(_, local),
                    ..
                } => {
                    let name = local.as_str().to_ascii_lowercase();
                    if !TAGS.contains(&name.as_str()) {
                        found.push(format!("</{name}>"));
                    }
                }
                Token::ElementEnd { .. } | Token::Text { .. } => {}
                _ => found.push("<!".to_owned()),
            }
        }
        found
    })
    .unwrap_or_else(|| vec!["<".to_owned()])
}

/// Whether `html` holds an `mj-*` open or close tag, in any letter case and under any namespace
/// prefix (`mrml` picks an element by its local name). `mj-include` is left to its own refusal.
/// Raw HTML goes inside an `mj-raw`, and one such tag could close it, close the column and the
/// section, and open structure of its own. The scan is by plain text, like the `mj-include`
/// scan, not by tokens: a fragment with an unmatched closing tag is not readable on its own, and
/// is exactly what must not pass. A comment that mentions a tag is refused too.
pub(crate) fn has_mjml_tag(html: &str) -> bool {
    let bytes = html.as_bytes();
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'<')
        .any(|(at, _)| {
            let rest = &bytes[at + 1..];
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            let end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'>'))
                .unwrap_or(rest.len());
            let name = &rest[..end];
            let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
            local.len() >= 3
                && local[..3].eq_ignore_ascii_case(b"mj-")
                && !local.eq_ignore_ascii_case(b"mj-include")
        })
}

/// Whether a URL has a scheme a link may use.
pub(crate) fn scheme_allowed(url: &str) -> bool {
    url_allowed(url, &|_| false)
}

/// Whether a URL source may be a link target: a merge tag first, or an allowed scheme.
pub(crate) fn link_allowed(url: &str) -> bool {
    starts_with_tag(url) || scheme_allowed(url)
}

/// The plain text of clean rich text: paragraphs apart by a blank line, list items on their own
/// line, line breaks kept, and a link as `label: url`.
pub(crate) fn html_to_text(html: &str) -> String {
    with_tokens(html, |tokens| {
        let mut out = String::new();
        let mut lists: Vec<Option<u32>> = Vec::new();
        let mut element = String::new();
        let mut href: Option<String> = None;
        let mut anchor_at = 0;
        for token in tokens {
            match token {
                Token::ElementStart { local, .. } => {
                    element = local.as_str().to_ascii_lowercase();
                    href = None;
                }
                Token::Attribute { local, value, .. } => {
                    if element == "a" && local.as_str().eq_ignore_ascii_case("href") {
                        href = value.as_ref().map(|value| unescape(value.as_str().trim()));
                    }
                }
                Token::ElementEnd {
                    end: ElementEnd::Open | ElementEnd::Empty,
                    ..
                } => match element.as_str() {
                    "p" => paragraph_break(&mut out),
                    "ul" => {
                        lists.push(None);
                        paragraph_break(&mut out);
                    }
                    "ol" => {
                        lists.push(Some(0));
                        paragraph_break(&mut out);
                    }
                    "li" => {
                        line_break_if_needed(&mut out);
                        match lists.last_mut() {
                            Some(Some(number)) => {
                                *number += 1;
                                out.push_str(&format!("{number}. "));
                            }
                            _ => out.push_str("- "),
                        }
                    }
                    "br" => out.push('\n'),
                    "a" => anchor_at = out.len(),
                    _ => {}
                },
                Token::ElementEnd {
                    end: ElementEnd::Close(_, local),
                    ..
                } => match local.as_str().to_ascii_lowercase().as_str() {
                    "p" => paragraph_break(&mut out),
                    "ul" | "ol" => {
                        lists.pop();
                        paragraph_break(&mut out);
                    }
                    "li" => line_break_if_needed(&mut out),
                    "a" => finish_link(&mut out, anchor_at, href.take()),
                    _ => {}
                },
                Token::Text { text } => push_text(&mut out, &unescape(text.as_str())),
                _ => {}
            }
        }
        tidy(&out)
    })
    .unwrap_or_default()
}

fn paragraph_break(out: &mut String) {
    out.truncate(out.trim_end_matches(' ').len());
    if !out.is_empty() && !out.ends_with("\n\n") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
}

fn line_break_if_needed(out: &mut String) {
    out.truncate(out.trim_end_matches(' ').len());
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Text with every run of whitespace as one space, and none at the start of a line.
fn push_text(out: &mut String, text: &str) {
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end_matches(char::is_whitespace);
        out.push_str(trimmed);
        if trimmed.len() != word.len() && !out.is_empty() && !out.ends_with(['\n', ' ']) {
            out.push(' ');
        }
    }
}

fn finish_link(out: &mut String, anchor_at: usize, href: Option<String>) {
    let Some(url) = href.filter(|url| !url.is_empty()) else {
        return;
    };
    out.truncate(out.trim_end().len().max(anchor_at));
    let label = out[anchor_at..].trim().to_owned();
    if label.is_empty() || label == url {
        out.truncate(anchor_at);
        out.push_str(&url);
    } else {
        out.push_str(": ");
        out.push_str(&url);
    }
}

/// Trailing spaces off every line, no run of more than one blank line, no blank edge.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank > 0 {
                out.push('\n');
            }
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn html(source: &str) -> String {
        sanitize_source(source).html
    }

    #[test]
    fn the_sanitizer_table_removes_what_is_not_allowed_and_keeps_what_is() {
        let nested = format!(
            "{}<a href=\"javascript:x\">x</a>{}",
            "<div>".repeat(256),
            "</div>".repeat(256)
        );
        let table = [
            // Scripts and what runs.
            ("<script>alert(1)</script><p>ok</p>", "<p>ok</p>"),
            ("<style>p{color:red}</style><p>ok</p>", "<p>ok</p>"),
            ("<p onclick=\"x()\">hi</p>", "<p>hi</p>"),
            ("<p onmouseover=x>hi</p>", "<p>hi</p>"),
            ("<p style=\"color:red\">hi</p>", "<p>hi</p>"),
            ("<img src=\"https://a/b.png\" onerror=\"x()\">", ""),
            ("<iframe src=\"https://a\"></iframe>", ""),
            ("<svg><script>x</script></svg>ok", "ok"),
            (
                "<noscript><img src=x onerror=alert(1)></noscript>",
                "&lt;img src=x onerror=alert(1)&gt;",
            ),
            (
                "<math><mi><script>x</script></mi></math><p>ok</p>",
                "<p>ok</p>",
            ),
            (
                "<template><img src=x onerror=alert(1)></template><p>ok</p>",
                "<p>ok</p>",
            ),
            ("<a><svg><script>x</script></svg>ok</a>", "<a>ok</a>"),
            (
                "<a href=\"https://a.example\">a<a href=\"javascript:x\">b</a></a>",
                "<a href=\"https://a.example\">a</a><a>b</a>",
            ),
            ("<p><b onclick=x>unclosed", "<p><b>unclosed</b></p>"),
            (
                "<P ONCLICK=x><A HREF=\"JAVASCRIPT:x\">x</A></P>",
                "<p><a>x</a></p>",
            ),
            ("<a href=\"java\0script:x\">x</a>", "<a>x</a>"),
            (nested.as_str(), "<a>x</a>"),
            ("<img src=\"data:image/svg+xml,<svg onload=alert(1)>\">", ""),
            (
                "<p srcdoc=\"<script>x</script>\" formaction=\"javascript:x\">ok</p>",
                "<p>ok</p>",
            ),
            (
                "<iframe srcdoc=\"<script>x</script>\"></iframe><button formaction=\"javascript:x\">ok</button>",
                "ok",
            ),
            (
                "<p style=\"background:url(javascript:alert(1))\">ok</p>",
                "<p>ok</p>",
            ),
            (
                "<p title=\"x\" lang=\"en\">ok</p><ol start=\"2\"><li>x</li></ol>",
                "<p>ok</p><ol><li>x</li></ol>",
            ),
            ("<p>a<!-- c -->b</p>", "<p>ab</p>"),
            // Links.
            ("<a href=\"javascript:alert(1)\">x</a>", "<a>x</a>"),
            ("<a href=\"JaVaScRiPt:alert(1)\">x</a>", "<a>x</a>"),
            ("<a href=\"java\tscript:alert(1)\">x</a>", "<a>x</a>"),
            ("<a href=\" javascript:alert(1)\">x</a>", "<a>x</a>"),
            (
                "<a href=\"\u{1}\t\r\njavascript:alert(1)\">x</a>",
                "<a>x</a>",
            ),
            ("<a href=\"javascript&colon;alert(1)\">x</a>", "<a>x</a>"),
            ("<a href=\"java&Tab;script:alert(1)\">x</a>", "<a>x</a>"),
            (
                "<a href=\"ftp://a.example\" hreflang=\"en\">x</a>",
                "<a>x</a>",
            ),
            ("<a href=\"data:text/html;base64,AAAA\">x</a>", "<a>x</a>"),
            ("<a href=\"vbscript:x\">x</a>", "<a>x</a>"),
            ("<a href=\"/relative\">x</a>", "<a>x</a>"),
            ("<a href=\"//evil.example/x\">x</a>", "<a>x</a>"),
            (
                "<a href=\"https://a.example/x?a=1&amp;b=2\" target=\"_blank\" onclick=\"x\">x</a>",
                "<a href=\"https://a.example/x?a=1&amp;b=2\">x</a>",
            ),
            (
                "<a href=\"http://a.example\">x</a>",
                "<a href=\"http://a.example\">x</a>",
            ),
            (
                "<a href=\"mailto:a@b.example\">x</a>",
                "<a href=\"mailto:a@b.example\">x</a>",
            ),
            (
                "<a href=\"{{ system.unsubscribe_url }}\">u</a>",
                "<a href=\"{{ system.unsubscribe_url }}\">u</a>",
            ),
            ("<a href=\"javascript:{{ x }}\">x</a>", "<a>x</a>"),
            // Allowed markup.
            (
                "<p>a <b>b</b> <strong>s</strong> <i>i</i> <em>e</em> <u>u</u></p>",
                "<p>a <b>b</b> <strong>s</strong> <i>i</i> <em>e</em> <u>u</u></p>",
            ),
            (
                "<ul><li>a</li><li>b</li></ul><ol><li>c</li></ol>",
                "<ul><li>a</li><li>b</li></ul><ol><li>c</li></ol>",
            ),
            ("<p>a<br>b</p>", "<p>a<br>b</p>"),
            // Other elements lose their markup, not their text.
            ("<div><h1>Hi</h1><span>there</span></div>", "Hithere"),
        ];
        for (input, expected) in table {
            assert_eq!(html(input), expected, "{input}");
            assert_eq!(html(expected), expected, "idempotence: {input}");
            assert!(violations(expected).is_empty(), "re-parse: {input}");
            if !input.contains("{{") {
                assert_eq!(clean_rendered(input), expected, "rendered: {input}");
            }
        }
    }

    fn hostile_html() -> impl Strategy<Value = String> {
        let tag = prop::sample::select(vec![
            "p", "b", "a", "br", "ul", "li", "div", "script", "style", "noscript", "math", "svg",
            "template", "iframe", "img", "form", "button",
        ]);
        let attribute = prop::sample::select(vec![
            "href",
            "src",
            "srcdoc",
            "formaction",
            "style",
            "onclick",
            "onerror",
            "title",
            "lang",
            "hreflang",
            "start",
        ]);
        let value = prop_oneof![
            prop::sample::select(vec![
                "https://safe.example/a",
                "http://safe.example",
                "mailto:a@b.example",
                "ftp://evil.example",
                "javascript:alert(1)",
                " javascript:x",
                "\t\njavascript:x",
                "java&Tab;script:x",
                "javascript&colon;x",
                "data:text/html,x",
                "/relative",
                "//evil.example",
                "background:url(javascript:x)",
            ])
            .prop_map(str::to_owned),
            "[a-zA-Z0-9 &;:/<>='\"\\t\\n]{0,40}",
        ];
        let element =
            (tag, attribute, value, any::<bool>()).prop_map(|(tag, attribute, value, close)| {
                format!(
                    "<{tag} {attribute}=\"{value}\">x{}",
                    if close {
                        format!("</{tag}>")
                    } else {
                        String::new()
                    }
                )
            });
        prop::collection::vec(prop_oneof![element, "[a-zA-Z0-9 &;<>/\0]{0,40}"], 0..16)
            .prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 128,
            rng_seed: proptest::test_runner::RngSeed::Fixed(389),
            ..ProptestConfig::default()
        })]

        #[test]
        fn sanitizing_is_idempotent(source in hostile_html()) {
            let once = sanitize_source(&source);
            prop_assert_eq!(sanitize_source(&once.html), once);
            let rendered = clean_rendered(&source);
            prop_assert_eq!(clean_rendered(&rendered), rendered);
        }

        #[test]
        fn sanitized_output_reparses_without_disallowed_tags_attributes_or_schemes(source in hostile_html()) {
            for output in [html(&source), clean_rendered(&source)] {
                prop_assert!(with_tokens(&output, |_| ()).is_some(), "cannot parse: {:?}", output);
                prop_assert!(check(&output, |_| false).is_empty(), "disallowed output: {:?}", output);
            }
        }
    }

    #[test]
    fn merge_tags_pass_through_the_sanitizer_untouched() {
        let source =
            "<p>{% if a > 1 and b == \"x\" %}{{ person.name | default(\"you\") }}{% endif %}</p>";
        assert_eq!(html(source), source);
        let list = "<ul>{% for t in event.tags %}<li>{{ t }}</li>{% endfor %}</ul>";
        assert_eq!(html(list), list);
    }

    #[test]
    fn violations_name_what_would_be_removed() {
        assert_eq!(violations("<p>ok <b>b</b></p>"), Vec::<String>::new());
        assert_eq!(
            violations("<p onclick=\"x\">a</p><script>x</script>"),
            ["onclick=", "<script>", "</script>"]
        );
        assert_eq!(violations("<a href=\"javascript:x\">a</a>"), ["href="]);
        assert_eq!(
            violations("<a href=\"{{ u }}\" title=\"t\">a</a>"),
            ["title="]
        );
        assert!(violations("<p>a<!-- c --></p>")[0].starts_with("<!"));
        assert_eq!(violations("a < b"), ["<"]);
        assert_eq!(
            violations("<a href=\"https://a.example\">a</a>"),
            Vec::<String>::new()
        );
        assert_eq!(violations("<a href=\"/x\">a</a>"), ["href="]);
        assert_eq!(violations("<a>no href</a>"), Vec::<String>::new());
        assert_eq!(violations("<IMG src=\"x\">"), ["<img>", "src="]);
        // Markup the tokenizer cannot read (an unquoted attribute) is outside the list too.
        assert_eq!(violations("<IMG src=x>"), ["<"]);
        // A root-closing trick leaves the wrapper's own tokens in the middle.
        assert_eq!(violations("</zqroot><p>x</p><zqroot>"), ["<"]);
    }

    #[test]
    fn a_rendered_text_that_is_clean_is_returned_as_it_is() {
        let clean = "<p>Hi &lt;b&gt;Ann&lt;/b&gt; &#x27;q&#x27; <a href=\"https://a.example/?a=1&amp;b=2\">x</a></p>";
        assert_eq!(enforce(clean), clean);
    }

    #[test]
    fn a_rendered_text_with_a_bad_link_is_cleaned() {
        assert_eq!(
            enforce("<p><a href=\"javascript:alert(1)\">x</a></p>"),
            "<p><a>x</a></p>"
        );
        assert_eq!(enforce("<p>a</p><script>x</script>"), "<p>a</p>");
        assert_eq!(enforce("<a href=\"/x\">x</a>"), "<a>x</a>");
        assert_eq!(enforce("<p>a < b</p>"), "<p>a &lt; b</p>");
    }

    #[test]
    fn link_sources_are_allowed_by_scheme_or_by_a_leading_merge_tag() {
        for url in [
            "https://a.example",
            "HTTP://a.example",
            "mailto:a@b.example",
            "{{ x }}",
            "{{ x }}/y",
        ] {
            assert!(link_allowed(url), "{url}");
        }
        for url in [
            "javascript:x",
            "data:x",
            "/x",
            "ftp://a",
            "",
            "tel:1",
            "x{{ y }}",
        ] {
            assert!(!link_allowed(url), "{url}");
        }
    }

    #[test]
    fn the_text_form_has_paragraphs_lists_breaks_and_links() {
        let text = |source: &str| sanitize_source(source).text;
        assert_eq!(text("<p>One</p><p>Two <b>bold</b></p>"), "One\n\nTwo bold");
        assert_eq!(text("<p>a<br>b</p>"), "a\nb");
        assert_eq!(
            text("<ul><li>x</li><li>y</li></ul><ol><li>p</li><li>q</li></ol>"),
            "- x\n- y\n\n1. p\n2. q"
        );
        assert_eq!(
            text("<p>See <a href=\"https://a.example/x?a=1&amp;b=2\">the page</a> now</p>"),
            "See the page: https://a.example/x?a=1&b=2 now"
        );
        assert_eq!(
            text("<p><a href=\"https://a.example\">https://a.example</a></p>"),
            "https://a.example"
        );
        assert_eq!(
            text("<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"),
            "Unsubscribe: {{ system.unsubscribe_url }}"
        );
        assert_eq!(text("<p>Tom &amp; Jerry &lt;3</p>"), "Tom & Jerry <3");
        assert_eq!(text("<p>Hello&nbsp;world</p>"), "Hello world");
        assert_eq!(text("<p>  spaced \n  out  </p>"), "spaced out");
        assert_eq!(text(""), "");
        assert_eq!(
            text("{% if a > 1 %}x{% endif %}"),
            "{% if a > 1 %}x{% endif %}"
        );
    }
}
