//! Finding the MiniJinja tags in a field, so the text around them can be treated as what it is.
//!
//! A field is a MiniJinja template. The text between its tags is literal: for a plain field it is
//! text that must be escaped before it goes into MJML, for rich text it is HTML. The tags
//! themselves are left alone. The scan is not a full lexer: it knows the three openers (`{{`,
//! `{%`, `{#`), their closers and `{% raw %}` blocks, and nothing else. A string that holds a
//! closer inside an expression (`{{ "}}" }}`) ends the tag early, and the rest is read as text.

use crate::links::escape;

/// A stretch of a template source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Segment<'a> {
    /// Literal text, output as it is.
    Text(&'a str),
    /// A tag, from its opener to its closer.
    Tag(&'a str),
}

fn closer(opener: u8) -> &'static str {
    match opener {
        b'{' => "}}",
        b'%' => "%}",
        _ => "#}",
    }
}

/// The name a `{% ... %}` tag opens, with the whitespace and whitespace-control marks removed.
fn statement(tag: &str) -> &str {
    tag.strip_prefix("{%")
        .and_then(|rest| rest.strip_suffix("%}"))
        .map_or("", |inner| {
            inner.trim_matches(|c: char| c.is_ascii_whitespace() || matches!(c, '-' | '+'))
        })
}

/// The span of the `{% endraw %}` tag after `from`, if there is one.
fn find_endraw(source: &str, from: usize) -> Option<(usize, usize)> {
    let mut at = from;
    while let Some(found) = source[at..].find("{%") {
        let start = at + found;
        let end = source[start..].find("%}").map(|close| start + close + 2)?;
        if statement(&source[start..end]) == "endraw" {
            return Some((start, end));
        }
        at = start + 2;
    }
    None
}

/// Split `source` into its text and its tags, in order. The segments are non-empty and, put
/// together, give `source` back.
pub(crate) fn split(source: &str) -> Vec<Segment<'_>> {
    let bytes = source.as_bytes();
    let mut segments = Vec::new();
    let (mut text_start, mut at) = (0, 0);
    while at + 1 < bytes.len() {
        let opener = bytes[at + 1];
        if bytes[at] != b'{' || !matches!(opener, b'{' | b'%' | b'#') {
            at += 1;
            continue;
        }
        let end = source[at + 2..]
            .find(closer(opener))
            .map_or(source.len(), |found| at + 2 + found + 2);
        if text_start < at {
            segments.push(Segment::Text(&source[text_start..at]));
        }
        let tag = &source[at..end];
        segments.push(Segment::Tag(tag));
        (at, text_start) = (end, end);
        if opener == b'%' && statement(tag) == "raw" {
            match find_endraw(source, end) {
                Some((start, close)) => {
                    if end < start {
                        segments.push(Segment::Text(&source[end..start]));
                    }
                    segments.push(Segment::Tag(&source[start..close]));
                    (at, text_start) = (close, close);
                }
                None => {
                    if end < source.len() {
                        segments.push(Segment::Text(&source[end..]));
                    }
                    return segments;
                }
            }
        }
    }
    if text_start < source.len() {
        segments.push(Segment::Text(&source[text_start..]));
    }
    segments
}

/// `source` with the text outside its tags escaped (see [`escape`]) and the tags as they
/// are. MiniJinja renders it with HTML autoescape, so a literal and a merged value both come out
/// as text.
pub(crate) fn escape_literals(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for segment in split(source) {
        match segment {
            Segment::Text(text) => out.push_str(&escape(text)),
            Segment::Tag(tag) => out.push_str(tag),
        }
    }
    out
}

/// Whether a URL source starts with a merge tag (`{{ ... }}`), so its scheme is only known at
/// render time.
pub(crate) fn starts_with_tag(url: &str) -> bool {
    url.trim_start().starts_with("{{")
}

/// A source with every tag replaced by a placeholder, so a sanitizer can read the rest as
/// plain markup without touching the tags.
pub(crate) struct Protected {
    prefix: String,
    tags: Vec<String>,
    /// The source with the placeholders in it.
    pub(crate) text: String,
}

impl Protected {
    pub(crate) fn new(source: &str) -> Self {
        // A prefix the source does not hold, so a placeholder can only be one of ours.
        let mut prefix = String::from("zq");
        while source.contains(&prefix) {
            prefix.push('q');
        }
        let mut tags = Vec::new();
        let mut text = String::with_capacity(source.len());
        for segment in split(source) {
            match segment {
                Segment::Text(literal) => text.push_str(literal),
                Segment::Tag(tag) => {
                    text.push_str(&format!("{prefix}{}x", tags.len()));
                    tags.push(tag.to_owned());
                }
            }
        }
        Self { prefix, tags, text }
    }

    /// Whether `url` starts with a placeholder: a URL that begins with a merge tag.
    pub(crate) fn starts_with_placeholder(&self, url: &str) -> bool {
        self.placeholder_at(url.trim_start())
            .is_some_and(|(index, _)| index < self.tags.len())
    }

    fn placeholder_at(&self, text: &str) -> Option<(usize, usize)> {
        let digits = text.strip_prefix(self.prefix.as_str())?;
        let count = digits.bytes().take_while(u8::is_ascii_digit).count();
        let index: usize = digits.get(..count)?.parse().ok()?;
        digits[count..]
            .starts_with('x')
            .then_some((index, self.prefix.len() + count + 1))
    }

    /// `text` with every placeholder replaced by its tag again.
    pub(crate) fn restore(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(found) = rest.find(self.prefix.as_str()) {
            out.push_str(&rest[..found]);
            rest = &rest[found..];
            match self
                .placeholder_at(rest)
                .and_then(|(index, used)| self.tags.get(index).map(|tag| (tag, used)))
            {
                Some((tag, used)) => {
                    out.push_str(tag);
                    rest = &rest[used..];
                }
                None => {
                    out.push_str(&rest[..self.prefix.len()]);
                    rest = &rest[self.prefix.len()..];
                }
            }
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Segment::{Tag, Text};
    use super::*;

    #[test]
    fn a_source_splits_into_text_and_tags_and_joins_back() {
        let source = "a {{ x }} b {% if y %}c{% endif %}{# n #}d";
        let segments = split(source);
        assert_eq!(
            segments,
            [
                Text("a "),
                Tag("{{ x }}"),
                Text(" b "),
                Tag("{% if y %}"),
                Text("c"),
                Tag("{% endif %}"),
                Tag("{# n #}"),
                Text("d"),
            ]
        );
        let joined: String = segments
            .iter()
            .map(|segment| match segment {
                Text(text) | Tag(text) => *text,
            })
            .collect();
        assert_eq!(joined, source);
        assert_eq!(split(""), []);
        assert_eq!(split("{ not } a tag {"), [Text("{ not } a tag {")]);
    }

    #[test]
    fn an_unterminated_tag_runs_to_the_end_so_the_engine_reports_it() {
        assert_eq!(split("a {{ x"), [Text("a "), Tag("{{ x")]);
        assert_eq!(split("{% if"), [Tag("{% if")]);
    }

    #[test]
    fn a_raw_block_is_text_between_its_two_tags() {
        assert_eq!(
            split("{% raw %}<b>{{ x }}</b>{%- endraw -%} {{ y }}"),
            [
                Tag("{% raw %}"),
                Text("<b>{{ x }}</b>"),
                Tag("{%- endraw -%}"),
                Text(" "),
                Tag("{{ y }}"),
            ]
        );
        // Without its end, everything after is text.
        assert_eq!(
            split("{% raw %}{{ x }}"),
            [Tag("{% raw %}"), Text("{{ x }}")]
        );
        assert_eq!(split("{% raw %}"), [Tag("{% raw %}")]);
        // Not a raw tag.
        assert_eq!(split("{% rawx %}a"), [Tag("{% rawx %}"), Text("a")]);
    }

    #[test]
    fn only_the_text_is_escaped() {
        assert_eq!(
            escape_literals(r#"Tom & "Jerry" <b> {{ a > 1 and b == "x" }} it's"#),
            r#"Tom &amp; &quot;Jerry&quot; &lt;b&gt; {{ a > 1 and b == "x" }} it's"#
        );
        assert_eq!(
            escape_literals("{% raw %}<i>{% endraw %}"),
            "{% raw %}&lt;i&gt;{% endraw %}"
        );
        assert_eq!(escape("a&b"), "a&amp;b");
    }

    #[test]
    fn a_url_starts_with_a_tag_only_when_its_first_characters_are_one() {
        assert!(starts_with_tag("{{ system.unsubscribe_url }}"));
        assert!(starts_with_tag("  {{ x }}/y"));
        assert!(!starts_with_tag("https://x/{{ y }}"));
        assert!(!starts_with_tag("{% if x %}https://a{% endif %}"));
    }

    #[test]
    fn protecting_swaps_tags_for_placeholders_and_restores_them() {
        let source = "<p>{{ a > 1 }}</p><a href=\"{{ u }}\">x</a>{% if k %}y{% endif %}";
        let protected = Protected::new(source);
        assert_eq!(protected.text, "<p>zq0x</p><a href=\"zq1x\">x</a>zq2xyzq3x");
        assert_eq!(protected.restore(&protected.text), source);
        assert!(protected.starts_with_placeholder(" zq1x/path"));
        assert!(!protected.starts_with_placeholder("zq9x"));
        assert!(!protected.starts_with_placeholder("https://a"));
        // Text that merely looks like a placeholder is kept.
        assert_eq!(protected.restore("zq zq99x zqx"), "zq zq99x zqx");
    }

    #[test]
    fn the_placeholder_prefix_is_one_the_source_does_not_use() {
        let source = "zq0x {{ a }} zqq1x";
        let protected = Protected::new(source);
        assert!(protected.text.contains("zqqq0x"), "{}", protected.text);
        assert_eq!(protected.restore(&protected.text), source);
    }
}
