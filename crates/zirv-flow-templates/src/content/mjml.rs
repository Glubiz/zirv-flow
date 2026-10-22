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
        doc.channel == Channel::Email
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
    /// `mrml` could not parse the text; where, and what kind of problem.
    Parse {
        line: usize,
        column: usize,
        reason: &'static str,
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

/// Whether `source` names an `mj-include` element anywhere, in any letter case. A comment that
/// mentions one is refused too: the check is a plain scan, not a parse.
pub(crate) fn has_include(source: &str) -> bool {
    const NEEDLE: &[u8] = b"<mj-include";
    source
        .as_bytes()
        .windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

/// The pinned `mrml` render options. Spelled out so a change of the crate's defaults cannot
/// change the bytes of a message.
fn render_options() -> RenderOptions {
    RenderOptions {
        disable_comments: false,
        ..RenderOptions::default()
    }
}

/// Turn MJML (already rendered by MiniJinja) into HTML.
pub(crate) fn compile(rendered: &str) -> Result<Compiled, MjmlError> {
    if has_include(rendered) {
        return Err(MjmlError::Include);
    }
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
        ParseError::UnexpectedElement { position, .. } => (position.start, "unexpected element"),
        ParseError::UnexpectedToken { position, .. } => (position.start, "unexpected token"),
        ParseError::MissingAttribute { position, .. } => (position.start, "missing attribute"),
        ParseError::InvalidAttribute { position, .. } => (position.start, "invalid attribute"),
        ParseError::InvalidFormat { position, .. } => (position.start, "invalid format"),
        ParseError::IncludeLoaderError { position, .. } => (position.start, "include refused"),
        ParseError::EndOfStream { .. } => (text.len(), "unexpected end"),
        ParseError::SizeLimit { .. } => (0, "text too large"),
        ParseError::ParserError { .. } => (0, "malformed markup"),
        ParseError::NoRootNode => (0, "no <mjml> element"),
    };
    let (line, column) = line_and_column(text, offset);
    MjmlError::Parse {
        line,
        column,
        reason,
    }
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
}
