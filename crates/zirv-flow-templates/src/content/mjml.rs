//! The `mjml` content type: code-mode email whose `html` field is MJML (architecture 11.5).
//!
//! The render order is fixed: MiniJinja renders the whole MJML source in the sandbox with HTML
//! autoescape on (so a merge value cannot add an MJML or HTML element), then `mrml` turns the
//! result into responsive HTML, then the post-render steps every email gets run on that HTML.
//!
//! `mrml` is built with its parse and render features only and is given a loader that finds
//! nothing, so an `mj-include` can read no file and fetch no URL. It is also refused by name
//! before `mrml` sees the text. The output is deterministic: `mrml` numbers its generated ids
//! with a per-render counter and hashes with a fixed hasher, and the render options are pinned
//! below.

use mrml::prelude::parser::noop_loader::NoopIncludeLoader;
use mrml::prelude::parser::{Error as ParseError, ParserOptions};
use mrml::prelude::render::RenderOptions;

use super::{RenderKind, TemplateContent};
use crate::document::{Channel, Mode, Syntax, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

pub(super) struct Mjml;

impl TemplateContent for Mjml {
    fn id(&self) -> &'static str {
        "mjml"
    }

    fn channel(&self) -> Channel {
        Channel::Email
    }

    fn handles(&self, doc: &TemplateDocument) -> bool {
        !doc.is_ambiguous()
            && doc.channel == Channel::Email
            && doc.mode == Mode::Code
            && doc
                .email
                .as_ref()
                .is_some_and(|email| email.syntax == Syntax::Mjml)
    }

    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
        crate::validate(doc, catalog)
    }

    fn render_kind(&self) -> RenderKind {
        RenderKind::Email
    }
}

/// Why MJML did not become HTML.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MjmlError {
    /// The source holds an `mj-include` element.
    Include,
    /// The elements nest deeper than [`MAX_DEPTH`].
    TooDeep,
    /// `mrml` could not parse the text; where, and what kind of problem.
    Parse {
        /// Line and column (1-based) in the text `mrml` read, which is the render of the source.
        line: usize,
        column: usize,
        reason: &'static str,
        /// A short, printable piece of that line around the column.
        excerpt: String,
    },
    /// `mrml` parsed the text but could not render it.
    Render,
}

/// MJML turned into HTML.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Compiled {
    pub(crate) html: String,
    /// The `lang` attribute of the `<mjml>` element, if it has a non-blank one.
    pub(crate) lang: Option<String>,
}

/// The deepest element nesting `mrml` is given. `mrml` 6.0.1 parses, renders and drops raw
/// elements recursively with no bound, so a few tens of thousands of nested tags overflow the
/// stack of the render host's child. Real MJML nests a dozen levels, and a table layout inside an
/// `mj-raw` a few dozen; 128 leaves several times that, and is far below the depth at which the
/// recursion gets near a default stack.
pub(crate) const MAX_DEPTH: usize = 128;

#[cfg(test)]
thread_local! {
    /// How many times `compile` reached `mrml`, so a test can show that a refusal came first.
    static PARSER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times `compile` reached `mrml` on this thread, for a test of a refusal that comes first.
#[cfg(test)]
pub(crate) fn parser_call_count() -> usize {
    PARSER_CALLS.with(std::cell::Cell::get)
}

/// Whether `source` has an `mj-include` element, in any letter case and under any namespace
/// prefix: `mrml` picks an element by its local name and ignores the prefix. A comment that
/// mentions one is refused too: the check is a plain scan, not a parse.
pub(crate) fn has_include(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'<')
        .any(|(at, _)| {
            let rest = &bytes[at + 1..];
            let end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'>'))
                .unwrap_or(rest.len());
            let name = &rest[..end];
            let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
            local.eq_ignore_ascii_case(b"mj-include")
        })
}

/// `mrml` 6.0.1's `is_void_element` (private there), mirrored exactly: the names are matched
/// case-sensitively, so `<BR>` is not void and `mrml` recurses into it.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// The deepest element nesting in `text`, counted over the tokens `mrml` itself reads (the same
/// `htmlparser` tokenizer, so conditional comments and every other construct are tokenized as
/// `mrml` tokenizes them): an open tag is +1 unless its local name is void, a closing tag is -1
/// (never below 0), and a self-closing tag and every other token is 0. The tokenizer is
/// iterative, so this is one linear pass with no recursion and no allocation per tag. At a
/// tokenizer error it stops and returns the depth so far, because `mrml` fails at the same point.
pub(crate) fn nesting_depth(text: &str) -> usize {
    use htmlparser::{ElementEnd, Token, Tokenizer};

    let (mut depth, mut deepest) = (0usize, 0usize);
    let mut void = false;
    for token in Tokenizer::from(text) {
        match token {
            Ok(Token::ElementStart { local, .. }) => void = is_void_element(local.as_str()),
            Ok(Token::ElementEnd {
                end: ElementEnd::Open,
                ..
            }) => {
                if !void {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
            }
            Ok(Token::ElementEnd {
                end: ElementEnd::Close(..),
                ..
            }) => depth = depth.saturating_sub(1),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    deepest
}

/// The pinned `mrml` render options. Spelled out so a change of the crate's defaults cannot
/// change the bytes of a message.
fn render_options() -> RenderOptions {
    RenderOptions {
        disable_comments: false,
        ..RenderOptions::default()
    }
}

/// Turn MJML (already rendered by MiniJinja) into HTML. Every caller of the `mrml` stage comes
/// through here, so the refusals before `mrml` runs are in one place.
pub(crate) fn compile(rendered: &str) -> Result<Compiled, MjmlError> {
    if has_include(rendered) {
        return Err(MjmlError::Include);
    }
    if nesting_depth(rendered) > MAX_DEPTH {
        return Err(MjmlError::TooDeep);
    }
    #[cfg(test)]
    PARSER_CALLS.with(|calls| calls.set(calls.get() + 1));
    let options = ParserOptions {
        include_loader: Box::new(NoopIncludeLoader),
    };
    let parsed = mrml::parse_with_options(rendered, &options)
        .map_err(|error| parse_error(rendered, &error))?;
    let html = parsed
        .element
        .render(&render_options())
        .map_err(|_| MjmlError::Render)?;
    let lang = parsed
        .element
        .attributes
        .lang
        .as_deref()
        .map(str::trim)
        .filter(|lang| !lang.is_empty())
        .map(str::to_owned);
    Ok(Compiled { html, lang })
}

fn parse_error(text: &str, error: &ParseError) -> MjmlError {
    let (offset, reason) = match error {
        // A backstop for the scan in `has_include`: the loader finds nothing, and that is a
        // refused include, not a malformed document.
        ParseError::IncludeLoaderError { .. } => return MjmlError::Include,
        ParseError::UnexpectedElement { position, .. } => (position.start, "unexpected element"),
        ParseError::UnexpectedToken { position, .. } => (position.start, "unexpected token"),
        ParseError::MissingAttribute { position, .. } => (position.start, "missing attribute"),
        ParseError::InvalidAttribute { position, .. } => (position.start, "invalid attribute"),
        ParseError::InvalidFormat { position, .. } => (position.start, "invalid format"),
        ParseError::EndOfStream { .. } => (text.len(), "unexpected end"),
        ParseError::SizeLimit { .. } => (0, "text too large"),
        // The tokenizer reports a 1-based row and column of its own.
        ParseError::ParserError { source, .. } => {
            let position = source.pos();
            let (line, column) = (position.row as usize, position.col as usize);
            return MjmlError::Parse {
                line,
                column,
                reason: "malformed markup",
                excerpt: excerpt(text, line, column),
            };
        }
        ParseError::NoRootNode => (0, "no <mjml> element"),
    };
    let (line, column) = line_and_column(text, offset);
    MjmlError::Parse {
        line,
        column,
        reason,
        excerpt: excerpt(text, line, column),
    }
}

/// At most 60 printable characters of line `line`, around `column`. Control characters are
/// dropped, so the text can sit in a message without breaking its line.
fn excerpt(text: &str, line: usize, column: usize) -> String {
    let content: Vec<char> = text
        .lines()
        .nth(line.saturating_sub(1))
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let start = column
        .saturating_sub(1)
        .saturating_sub(20)
        .min(content.len());
    content[start..].iter().take(60).collect()
}

/// The 1-based line and column (in characters) of a byte offset.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{EmailContent, WebhookContent};

    const MINIMAL: &str = r#"<mjml lang="en"><mj-body><mj-section><mj-column><mj-text>Hi</mj-text></mj-column></mj-section></mj-body></mjml>"#;

    #[test]
    fn it_owns_mjml_syntax_email_only() {
        let mut doc = TemplateDocument::email_code(EmailContent::default());
        assert!(!Mjml.handles(&doc));
        if let Some(email) = doc.email.as_mut() {
            email.syntax = Syntax::Mjml;
        }
        assert!(Mjml.handles(&doc));
        assert!(!super::super::html::Html.handles(&doc));
        assert!(!Mjml.handles(&TemplateDocument::webhook_code(WebhookContent::default())));
        doc.mode = Mode::Blocks;
        assert!(!Mjml.handles(&doc));
        assert_eq!(Mjml.id(), "mjml");
        assert_eq!(Mjml.render_kind(), RenderKind::Email);
    }

    #[test]
    fn it_compiles_mjml_and_reads_the_language() {
        let compiled = compile(MINIMAL).unwrap();
        assert!(compiled.html.contains("Hi"));
        assert_eq!(compiled.lang.as_deref(), Some("en"));
        let bare = compile("<mjml><mj-body></mj-body></mjml>").unwrap();
        assert_eq!(bare.lang, None);
        assert_eq!(compile(MINIMAL).unwrap(), compiled);
    }

    #[test]
    fn it_refuses_an_include_in_any_case_before_parsing() {
        for source in [
            r#"<mjml><mj-body><mj-include path="a.mjml" /></mj-body></mjml>"#,
            r#"<mjml><mj-body><MJ-INCLUDE path="/etc/passwd" /></mj-body></mjml>"#,
            "<mjml><mj-body><mj-include",
        ] {
            assert_eq!(compile(source), Err(MjmlError::Include), "{source}");
        }
        assert!(!has_include(MINIMAL));
    }

    #[test]
    fn an_include_is_found_by_its_local_name_and_a_tag_boundary() {
        for source in [
            r#"<mjml><mj-body><x:mj-include path="a"/></mj-body></mjml>"#,
            r#"<mjml><mj-body><MJ-INCLUDE></MJ-INCLUDE></mj-body></mjml>"#,
            "<mjml><mj-body><a:b:mj-include\npath=\"a\"/></mj-body></mjml>",
        ] {
            assert!(has_include(source), "{source}");
            assert_eq!(compile(source), Err(MjmlError::Include), "{source}");
        }
        // A longer name is another element: the include check leaves it to `mrml`.
        for source in ["<mj-includex/>", "<mj-include-all/>", "<x:mj-includes />"] {
            assert!(!has_include(source), "{source}");
        }
    }

    fn parser_calls() -> usize {
        PARSER_CALLS.with(std::cell::Cell::get)
    }

    #[test]
    fn nesting_too_deep_is_refused_before_mrml_runs() {
        // Deep enough that `mrml` would recurse far past a thread's stack if it were called.
        let deep = format!(
            "<mjml><mj-body><mj-raw>{}</mj-raw></mj-body></mjml>",
            "<a>".repeat(35_000)
        );
        assert!(nesting_depth(&deep) > MAX_DEPTH);
        let before = parser_calls();
        assert_eq!(compile(&deep), Err(MjmlError::TooDeep));
        assert_eq!(parser_calls(), before, "mrml must not see it");
        // The same through the render and the checks.
        let doc = mjml_doc(Category::Transactional, &deep);
        assert_eq!(render(&doc), Err(RenderError::MjmlTooDeep));
        let issues = validate(&doc, &catalog());
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == code::MJML_TOO_DEEP && issue.is_blocking()),
            "{issues:?}"
        );
    }

    #[test]
    fn real_nesting_passes_the_depth_guard() {
        for (name, _) in CORPUS {
            assert!(nesting_depth(&source(name)) < 16, "{name}");
        }
        let table = "<table><tr><td>".repeat(40) + "x" + &"</td></tr></table>".repeat(40);
        let layout = format!(
            "<mjml lang=\"en\"><mj-body><mj-section><mj-column><mj-raw>{table}<br><img src=\"a\"></mj-raw></mj-column></mj-section></mj-body></mjml>"
        );
        let depth = nesting_depth(&layout);
        assert!(depth > 100, "{depth}");
        assert!(depth <= MAX_DEPTH, "{depth}");
        let before = parser_calls();
        assert!(compile(&layout).is_ok());
        assert_eq!(parser_calls(), before + 1);
    }

    #[test]
    fn the_depth_scan_counts_open_close_and_self_closing_tags() {
        assert_eq!(nesting_depth("<a><b></b><c/><br></a>"), 2);
        assert_eq!(nesting_depth("<r></a></b><a>"), 1);
        assert_eq!(
            nesting_depth("<!-- <a><a><a> --><a x=\">\"><![CDATA[<b><b>]]></a>"),
            1
        );
        assert_eq!(nesting_depth("<a><b"), 1);
        assert_eq!(nesting_depth(""), 0);
    }

    fn deep_inside(opening: &str, repeated: &str) -> String {
        format!(
            "<mjml><mj-body><mj-raw>{opening}{}</mj-raw></mj-body></mjml>",
            repeated.repeat(35_000)
        )
    }

    #[test]
    fn conditional_comment_tokens_do_not_hide_nesting_from_the_guard() {
        for opening in [
            "<!--[if mso]>",
            "<![if mso]>",
            "<![endif]>",
            "<!--<![endif]-->",
            "<!--[if mso]><![endif]-->",
            // Closed or not, the tokens after it are what `mrml` reads.
            "<!--[if mso]>x<![endif]-->",
        ] {
            let deep = deep_inside(opening, "<a>");
            assert!(nesting_depth(&deep) > MAX_DEPTH, "{opening}");
            let before = parser_calls();
            assert_eq!(compile(&deep), Err(MjmlError::TooDeep), "{opening}");
            assert_eq!(parser_calls(), before, "{opening}: mrml must not see it");
        }
    }

    #[test]
    fn void_names_are_matched_the_way_mrml_matches_them() {
        for tag in ["<BR>", "<IMG>", "<HR>", "<Br>"] {
            let deep = deep_inside("", tag);
            assert!(
                nesting_depth(&deep) > MAX_DEPTH,
                "{tag} is not void for mrml"
            );
            assert_eq!(compile(&deep), Err(MjmlError::TooDeep), "{tag}");
        }
        for tag in ["<br>", "<img src=\"a\">", "<hr/>", "<x:br>"] {
            let flat = deep_inside("", tag);
            assert!(nesting_depth(&flat) <= 4, "{tag}");
        }
    }

    #[test]
    fn a_real_conditional_comment_block_still_passes() {
        let source = "<mjml lang=\"en\"><mj-body><mj-section><mj-column><mj-raw>\
            <!--[if mso | IE]><table role=\"presentation\"><tr><td><![endif]-->\
            <div><p>Hi<br></p></div>\
            <!--[if mso | IE]></td></tr></table><![endif]-->\
            </mj-raw></mj-column></mj-section></mj-body></mjml>";
        assert!(nesting_depth(source) < 16);
        assert!(compile(source).is_ok());
    }

    #[test]
    fn a_parse_error_message_names_the_sample_render_and_a_clean_excerpt() {
        let source = "<mjml lang=\"en\">\n<mj-body>\n  <mj-text>\n</mj-body>\u{7}</mjml>";
        let issues = issues_of(source);
        let message = issues
            .iter()
            .find(|issue| issue.code == code::MJML_PARSE_ERROR)
            .and_then(|issue| issue.message.clone())
            .expect("a parse error message");
        assert!(message.contains("line 4, column "), "{message}");
        assert!(message.contains("of the sample render"), "{message}");
        assert!(message.contains("near: </mj-body>"), "{message}");
        assert!(!message.chars().any(char::is_control), "{message}");

        let long = format!("<mjml>{}<mj-text></mj-body></mjml>", "x".repeat(200));
        match compile(&long) {
            Err(MjmlError::Parse { excerpt, .. }) => assert!(excerpt.chars().count() <= 60),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn the_missing_lang_warning_says_to_set_it_on_the_mjml_element() {
        let issues = issues_of("<mjml><mj-body></mj-body></mjml>");
        let warning = issues
            .iter()
            .find(|issue| issue.code == code::MISSING_LANG)
            .expect("missing_lang");
        let message = warning.message.as_deref().unwrap_or("");
        assert!(
            message.contains("`lang` on the <mjml> element"),
            "{message}"
        );
        assert!(message.contains("not used for MJML"), "{message}");
    }

    #[test]
    fn a_parse_error_names_its_line_and_column() {
        let source = "<mjml>
<mj-body>
  <mj-text>
</mj-body></mjml>";
        match compile(source) {
            Err(MjmlError::Parse { line, column, .. }) => {
                assert_eq!(line, 4, "{source}");
                assert!(column >= 1);
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        assert!(matches!(compile("not mjml"), Err(MjmlError::Parse { .. })));
    }

    #[test]
    fn line_and_column_count_characters() {
        assert_eq!(line_and_column("ab\ncé x", 7), (2, 4));
        assert_eq!(line_and_column("abc", 99), (1, 4));
    }

    // ----- the golden corpus and the render path (#237, OQ-25) -----------------------------

    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    use serde_json::json;

    use crate::document::Category;
    use crate::error::RenderError;
    use zirv_flow_core::people::TraitType;

    use crate::issue::{Issue, Severity, code};
    use crate::links::{Hooks, Tracking};
    use crate::render::{Rendered, apply_tracking, render_email};
    use crate::{RenderContext, validate};

    const UNSUBSCRIBE: &str = "https://u.example/u/1?s=a&t=b";

    /// The fixtures: name and category. Only the footer carries the unsubscribe link, so the
    /// others are written as transactional mail.
    const CORPUS: [(&str, Category); 6] = [
        ("one_column", Category::Transactional),
        ("two_columns", Category::Transactional),
        ("three_columns", Category::Transactional),
        ("hero_image", Category::Transactional),
        ("button", Category::Transactional),
        ("footer_unsubscribe", Category::Marketing),
    ];

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mjml")
    }

    fn source(name: &str) -> String {
        fs::read_to_string(fixtures().join(format!("{name}.mjml")))
            .unwrap()
            .replace("\r\n", "\n")
    }

    fn mjml_doc(category: Category, source: &str) -> TemplateDocument {
        TemplateDocument::email_code(EmailContent {
            subject: "Hi {{ person.name }}".into(),
            sender: Some("snd_1".into()),
            lang: None,
            category,
            syntax: Syntax::Mjml,
            html: source.into(),
            text: "Hello {{ person.name }}".into(),
        })
    }

    fn context() -> RenderContext {
        let mut context = RenderContext::default();
        context.person.insert("name".into(), json!("Ann"));
        context.system.unsubscribe_url = UNSUBSCRIBE.into();
        context.system.preferences_url = "https://u.example/p/1".into();
        context
    }

    fn render(doc: &TemplateDocument) -> Result<Rendered, RenderError> {
        render_email(doc, &context(), &Hooks::untracked())
    }

    fn check(name: &str, actual: &str) {
        let path = fixtures().join(format!("{name}.html"));
        if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
            fs::write(&path, actual).unwrap();
        }
        let expected = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing {}; bless with ZIRV_FLOW_BLESS=1", path.display()));
        assert_eq!(expected.replace("\r\n", "\n"), actual, "{name}");
    }

    fn catalog() -> FieldCatalog {
        FieldCatalog {
            person: std::collections::BTreeMap::from([("name".to_owned(), TraitType::String)]),
            ..FieldCatalog::default()
        }
    }

    fn corpus_html(name: &str, category: Category) -> String {
        render(&mjml_doc(category, &source(name))).unwrap().html
    }

    #[test]
    fn the_golden_corpus_renders_to_its_expected_bytes_twice() {
        for (name, category) in CORPUS {
            let first = corpus_html(name, category);
            check(name, &first);
            assert_eq!(first, corpus_html(name, category), "{name} is not stable");
        }
    }

    #[test]
    fn every_corpus_document_validates_without_a_blocking_issue() {
        for (name, category) in CORPUS {
            let issues = validate(&mjml_doc(category, &source(name)), &catalog());
            let blocking: Vec<_> = issues.iter().filter(|issue| issue.is_blocking()).collect();
            assert!(blocking.is_empty(), "{name}: {blocking:?}");
        }
    }

    #[test]
    fn the_output_is_table_layout_with_inline_styles_mso_comments_and_a_mobile_query() {
        for (name, category) in CORPUS {
            let html = corpus_html(name, category);
            assert!(html.starts_with("<!doctype html>"), "{name}");
            assert!(html.contains("<table"), "{name}: table-based layout");
            assert!(html.contains(" style=\""), "{name}: inline styles");
            assert!(html.contains("<!--[if mso"), "{name}: conditional comments");
            assert!(html.contains("<![endif]-->"), "{name}");
            assert!(
                html.contains("@media only screen and (min-width:480px)"),
                "{name}: mobile media query"
            );
            assert!(html.contains("max-width:600px"), "{name}: content width");
            assert!(html.contains("lang=\"en\""), "{name}");
        }
    }

    #[test]
    fn columns_split_the_content_width() {
        assert!(corpus_html("one_column", Category::Transactional).contains("mj-column-per-100"));
        let two = corpus_html("two_columns", Category::Transactional);
        assert!(two.contains("mj-column-per-50"));
        let three = corpus_html("three_columns", Category::Transactional);
        assert!(three.contains("mj-column-per-33"));
    }

    #[test]
    fn a_hero_image_and_a_button_keep_their_urls() {
        let hero = corpus_html("hero_image", Category::Transactional);
        assert!(hero.contains("https://img.example/hero.png"));
        assert!(hero.contains("https://img.example/banner.png"));
        let button = corpus_html("button", Category::Transactional);
        assert!(button.contains("href=\"https://shop.example/offer\""));
    }

    #[test]
    fn an_include_is_refused_and_nothing_is_read() {
        let path = fixtures().join("one_column.mjml");
        let include = format!(
            "<mjml lang=\"en\"><mj-body><mj-include path=\"{}\" /></mj-body></mjml>",
            path.display().to_string().replace('\\', "/")
        );
        let doc = mjml_doc(Category::Transactional, &include);
        assert_eq!(render(&doc), Err(RenderError::MjmlInclude));
        let issues = validate(&doc, &catalog());
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == code::MJML_INCLUDE && issue.is_blocking()),
            "{issues:?}"
        );
        // The refusal comes before `mrml` sees the text, so even a loader that would find the
        // file never runs: the error is ours, not the loader's.
        assert_eq!(compile(&include), Err(MjmlError::Include));
        // An include in a branch that does not run is still refused when validating.
        let hidden = "{% if false %}<mj-include path=\"x\" />{% endif %}<mjml lang=\"en\"><mj-body /></mjml>";
        let hidden_doc = mjml_doc(Category::Transactional, hidden);
        assert_eq!(render(&hidden_doc), Err(RenderError::MjmlInclude));
    }

    #[test]
    fn a_merge_value_with_markup_is_escaped_and_adds_no_element() {
        let mut context = context();
        context.person.insert(
            "name".into(),
            json!(
                r#"</mj-text><mj-include path="x"/><mj-text onclick="x"><script>alert(1)</script>"#
            ),
        );
        let doc = mjml_doc(Category::Transactional, &source("one_column"));
        let rendered = render_email(&doc, &context, &Hooks::untracked()).unwrap();
        assert!(!rendered.html.contains("<script"), "{}", rendered.html);
        assert!(!rendered.html.contains("<mj-"), "{}", rendered.html);
        assert!(rendered.html.contains("&lt;script&gt;"));
        assert!(!rendered.html.contains("onclick=\"x\">"));
        // The same document rendered with a plain name has the same structure.
        let plain = render(&doc).unwrap().html;
        assert_eq!(
            plain.matches("<table").count(),
            rendered.html.matches("<table").count()
        );
    }

    fn issues_of(source: &str) -> Vec<Issue> {
        validate(&mjml_doc(Category::Transactional, source), &catalog())
    }

    #[test]
    fn an_mrml_parse_error_is_blocking_and_names_its_position() {
        let issues = issues_of("<mjml lang=\"en\">\n<mj-body>\n<mj-text>\n</mj-body></mjml>");
        let found = issues
            .iter()
            .find(|issue| issue.code == code::MJML_PARSE_ERROR)
            .expect("a parse error");
        assert_eq!(found.severity, Severity::Blocking);
        assert_eq!(found.at, "/email/html");
        assert!(
            found
                .message
                .as_deref()
                .is_some_and(|message| message.contains("line 4")),
            "{found:?}"
        );
        let doc = mjml_doc(Category::Transactional, "<mjml><mj-body>");
        assert_eq!(render(&doc), Err(RenderError::Mjml));
    }

    #[test]
    fn a_marketing_template_without_an_unsubscribe_link_is_refused() {
        let doc = mjml_doc(Category::Marketing, &source("one_column"));
        assert_eq!(render(&doc), Err(RenderError::MissingUnsubscribe));
        let issues = validate(&doc, &catalog());
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == code::MISSING_UNSUBSCRIBE && issue.is_blocking()),
            "{issues:?}"
        );
        // The link in the footer fixture satisfies it, in the HTML `mrml` made.
        let ok = mjml_doc(Category::Marketing, &source("footer_unsubscribe"));
        assert!(
            !validate(&ok, &catalog())
                .iter()
                .any(|issue| issue.code == code::MISSING_UNSUBSCRIBE)
        );
    }

    #[test]
    fn a_link_that_only_the_source_names_does_not_count_as_unsubscribe() {
        // The URL is in a comment and an attribute that is not a link, so no anchor reaches it.
        let source = "<mjml lang=\"en\"><mj-body><mj-section><mj-column><mj-text>\
            <!-- {{ system.unsubscribe_url }} --></mj-text></mj-column></mj-section></mj-body></mjml>";
        let doc = mjml_doc(Category::Marketing, source);
        assert_eq!(render(&doc), Err(RenderError::MissingUnsubscribe));
    }

    fn tok(index: usize, _url: &str) -> String {
        format!("tok{index}")
    }

    #[test]
    fn link_rewrite_and_list_unsubscribe_apply_to_the_mrml_output() {
        let doc = mjml_doc(Category::Marketing, &source("footer_unsubscribe"));
        let live = Hooks {
            tracking: Tracking::Rewrite {
                base_url: "https://t.example/c",
                token: &tok,
            },
        };
        let direct = render_email(&doc, &context(), &live).unwrap();
        assert_eq!(direct.headers.len(), 2);
        assert!(direct.headers[0].0.eq_ignore_ascii_case("List-Unsubscribe"));
        assert!(direct.headers[0].1.contains(UNSUBSCRIBE));
        // The message's own links are left alone.
        assert!(!direct.html.contains("t.example"), "{}", direct.html);
        assert!(crate::links::links_to(&direct.html, UNSUBSCRIBE));

        let button = mjml_doc(Category::Transactional, &source("button"));
        let tracked = render_email(&button, &context(), &live).unwrap();
        assert!(
            tracked.html.contains("https://t.example/c"),
            "{}",
            tracked.html
        );
        assert!(!tracked.html.contains("href=\"https://shop.example/offer\""));

        // Split the way the isolated host does: untracked render, then the caller's rewrite.
        let untracked = render(&button).unwrap();
        let finished = apply_tracking(untracked, &context(), &live).unwrap();
        assert_eq!(finished, tracked);
    }

    #[test]
    fn a_missing_lang_an_insecure_link_and_a_large_output_are_warnings() {
        let no_lang = "<mjml><mj-body><mj-section><mj-column>\
            <mj-button href=\"http://shop.example/x\">Go</mj-button>\
            </mj-column></mj-section></mj-body></mjml>";
        let issues = issues_of(no_lang);
        for wanted in [code::MISSING_LANG, code::INSECURE_LINK] {
            let found = issues.iter().find(|issue| issue.code == wanted);
            assert_eq!(
                found.map(|issue| issue.severity),
                Some(Severity::Warning),
                "{wanted}"
            );
        }
        assert!(
            issues.iter().all(|issue| !issue.is_blocking()),
            "{issues:?}"
        );

        // Rendered HTML near the clip size: a long text makes the sample output large.
        let big = format!(
            "<mjml lang=\"en\"><mj-body><mj-section><mj-column><mj-text>{}</mj-text></mj-column></mj-section></mj-body></mjml>",
            "word ".repeat(20_000)
        );
        let issues = issues_of(&big);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == code::HTML_NEAR_CLIP)
        );
        assert!(
            issues.iter().all(|issue| !issue.is_blocking()),
            "{issues:?}"
        );
        // A clean document has neither.
        let clean = issues_of(&source("one_column"));
        assert!(
            !clean
                .iter()
                .any(|issue| issue.code == code::MISSING_LANG || issue.code == code::INSECURE_LINK)
        );
    }

    #[test]
    fn html_documents_are_unchanged_by_the_syntax_field() {
        let stored = r#"{"schema":1,"channel":"email","mode":"code","email":{"subject":"Hi","html":"<p>x</p>"}}"#;
        let doc: TemplateDocument = serde_json::from_str(stored).unwrap();
        assert_eq!(doc.email.as_ref().map(|e| e.syntax), Some(Syntax::Html));
        assert!(!serde_json::to_string(&doc).unwrap().contains("syntax"));
        let mjml = mjml_doc(Category::Transactional, "<mjml />");
        let json = serde_json::to_string(&mjml).unwrap();
        assert!(json.contains(r#""syntax":"mjml""#));
        assert_eq!(
            serde_json::from_str::<TemplateDocument>(&json).unwrap(),
            mjml
        );
    }

    /// The per-message cost of the MJML stage: renders the corpus many times and prints the
    /// median and the 95th percentile of one render. Run on demand:
    /// `cargo nextest run -p zirv-flow-templates --run-ignored only --no-capture per_render_cost`.
    #[test]
    #[ignore = "timing; run on demand"]
    fn per_render_cost_of_the_representative_set() {
        const ROUNDS: usize = 200;
        let docs: Vec<TemplateDocument> = CORPUS
            .iter()
            .map(|(name, category)| mjml_doc(*category, &source(name)))
            .collect();
        let context = context();
        let hooks = Hooks::untracked();
        let mut micros = Vec::with_capacity(ROUNDS * docs.len());
        for _ in 0..ROUNDS {
            for doc in &docs {
                let started = Instant::now();
                let rendered = render_email(doc, &context, &hooks);
                micros.push(started.elapsed().as_micros());
                assert!(rendered.is_ok());
            }
        }
        micros.sort_unstable();
        let at = |fraction: usize| micros[micros.len() * fraction / 100 - 1];
        println!(
            "mjml render, {} renders over {} documents: p50 {} us, p95 {} us",
            micros.len(),
            docs.len(),
            at(50),
            at(95)
        );
    }
}
