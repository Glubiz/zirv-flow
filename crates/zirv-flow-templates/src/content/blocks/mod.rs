//! The `blocks` content type: the block document v1 and its renderer (architecture 11.1, 11.2,
//! 11.5, 11.6).
//!
//! A block email is a tree of sections, columns and blocks plus a few style settings. A render
//! goes through MiniJinja for each text field, MJML emitted from the tree, `mrml`, and a text
//! part built from the tree; [`blocks_to_mjml`] is the same walk without the first step, so a
//! marketer can leave the builder for MJML code with the merge tags intact.

mod check;
mod emit;
mod look;
mod model;
mod render;
mod sanitize;
mod snippet;
mod tags;

use std::fmt;

use super::{RenderKind, TemplateContent};
use crate::context::RenderExtras;
use crate::document::{Channel, Mode, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

pub(crate) use check::validate;
pub use model::{
    Align, Block, BlocksContent, BlocksError, Button, Divider, FONT_STACKS, FooterRef, Heading,
    Image, ImageSrc, MAX_BLOCKS, MAX_COLUMNS, MAX_CONDITION_BYTES, MAX_CONDITION_DEPTH,
    MAX_CONTENT_WIDTH, MAX_DOCUMENT_BYTES, MAX_ID_BYTES, MAX_IMAGE_WIDTH, MAX_LABEL_BYTES,
    MAX_RAW_HTML_BYTES, MAX_RICH_TEXT_BYTES, MAX_SECTIONS, MAX_SPACER_HEIGHT,
    MAX_SUBJECT_SOURCE_BYTES, MAX_URL_BYTES, MIN_CONTENT_WIDTH, PREHEADER_DISPLAY_CHARS, RawHtml,
    RichText, SUBJECT_DISPLAY_CHARS, Section, Settings, Snippet, Spacer, ThemeRef, ThemeSlot,
    conditions, parse_template_document,
};
pub(crate) use model::{font_css, is_color};
pub(crate) use render::render;
pub use snippet::{MAX_SNIPPET_BLOCKS, SnippetContent, validate_snippet};

pub(super) struct Blocks;

impl TemplateContent for Blocks {
    fn id(&self) -> &'static str {
        "blocks"
    }

    fn channel(&self) -> Channel {
        Channel::Email
    }

    fn handles(&self, doc: &TemplateDocument) -> bool {
        !doc.is_ambiguous()
            && doc.channel == Channel::Email
            && doc.mode == Mode::Blocks
            && doc.blocks.is_some()
    }

    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
        validate(doc, catalog, &RenderExtras::default())
    }

    fn validate_with(
        &self,
        doc: &TemplateDocument,
        catalog: &FieldCatalog,
        extras: &RenderExtras,
    ) -> Vec<Issue> {
        validate(doc, catalog, extras)
    }

    fn render_kind(&self) -> RenderKind {
        RenderKind::Email
    }
}

/// A block document turned into MJML code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Converted {
    /// The MJML source, with every merge tag as the marketer wrote it. Rendered through the MJML
    /// path (MiniJinja over the whole source, then `mrml`) it gives the HTML of the block render.
    pub mjml: String,
    /// The text part of the tree, merge tags kept.
    pub text: String,
}

/// The document could not be converted because its blocking checks fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvertError {
    /// The blocking issues, in document order.
    pub issues: Vec<Issue>,
}

impl fmt::Display for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the block document has {} blocking issue(s)",
            self.issues.len()
        )
    }
}

impl std::error::Error for ConvertError {}

/// Convert a block document to MJML source and a text part. One way: nothing turns MJML back
/// into blocks. The checks that need no render (shape, limits, URLs, the sanitizer) must pass;
/// merge fields and syntax are the converted code's business afterwards.
///
/// MJML code has no place for a condition, an asset, a theme image or a snippet, so a document
/// that uses one is refused with `not_convertible`.
pub fn blocks_to_mjml(content: &BlocksContent) -> Result<Converted, ConvertError> {
    let mut issues: Vec<Issue> = check::check_sources(content, &RenderExtras::default())
        .into_iter()
        .filter(Issue::is_blocking)
        .collect();
    issues.extend(check::not_convertible(content));
    if !issues.is_empty() {
        return Err(ConvertError { issues });
    }
    let look = look::Look::of_settings(&content.settings);
    let Ok(emitted) = emit::emit(content, &look, &mut emit::Sources);
    Ok(Converted {
        mjml: emitted.mjml,
        text: emitted.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::EmailContent;

    #[test]
    fn it_owns_block_mode_email_with_content_only() {
        let doc = TemplateDocument::email_blocks(BlocksContent::new("Hi"));
        assert!(Blocks.handles(&doc));
        assert_eq!(Blocks.id(), "blocks");
        assert_eq!(Blocks.channel(), Channel::Email);
        assert_eq!(Blocks.render_kind(), RenderKind::Email);

        let mut without = doc.clone();
        without.blocks = None;
        assert!(!Blocks.handles(&without));
        let mut code = doc.clone();
        code.mode = Mode::Code;
        assert!(!Blocks.handles(&code));
        let mut webhook = doc;
        webhook.channel = Channel::Webhook;
        assert!(!Blocks.handles(&webhook));
        assert!(!Blocks.handles(&TemplateDocument::email_code(EmailContent::default())));
    }

    /// A marketing code `email` beside block content that says transactional: the category check
    /// of a caller that reads `email` first must not decide for a renderer that reads `blocks`.
    fn two_contents() -> serde_json::Value {
        serde_json::json!({
            "schema": 2, "channel": "email", "mode": "blocks",
            "email": {"subject": "x", "html": "", "category": "marketing"},
            "blocks": {"subject": "Hi", "category": "transactional", "sender": "0192b1a4-7c3e-7000-8000-00000000a001", "lang": "en"}
        })
    }

    #[test]
    fn a_document_with_two_contents_is_refused_by_every_entry_point() {
        use crate::{FieldCatalog, RenderContext, RenderError, code, render_email, validate};

        let value = two_contents();
        assert_eq!(
            parse_template_document(&value).unwrap_err(),
            BlocksError::AmbiguousContent
        );
        // Built by serde alone, bypassing the typed parse, it is still not owned or rendered.
        let doc: TemplateDocument = serde_json::from_value(value).unwrap();
        assert!(doc.is_ambiguous());
        assert_eq!(doc.content_count(), 2);
        assert!(!Blocks.handles(&doc));
        assert_eq!(
            crate::content_type_of(&doc).err(),
            Some(crate::ContentError::UnsupportedDocument)
        );
        assert_eq!(
            render_email(&doc, &RenderContext::default(), &crate::Hooks::untracked()),
            Err(RenderError::WrongDocument)
        );
        let issues = validate(&doc, &FieldCatalog::default());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, code::AMBIGUOUS_CONTENT);
        assert!(issues[0].is_blocking());
        // The same for a code-mode pair.
        let mut code_doc = doc.clone();
        code_doc.mode = Mode::Code;
        assert_eq!(
            render_email(
                &code_doc,
                &RenderContext::default(),
                &crate::Hooks::untracked()
            ),
            Err(RenderError::WrongDocument)
        );
    }

    #[test]
    fn an_unknown_root_field_is_a_typed_error() {
        let mut value = serde_json::json!({"schema": 2, "channel": "email", "mode": "blocks",
                                           "blocks": {"subject": "Hi"}});
        assert!(parse_template_document(&value).is_ok());
        value["palette"] = serde_json::json!("dark");
        assert_eq!(
            parse_template_document(&value).unwrap_err(),
            BlocksError::UnknownField
        );
        // A bad enum value on the root is malformed, not an unknown block.
        let bad = serde_json::json!({"schema": 1, "channel": "fax", "mode": "code"});
        assert_eq!(
            parse_template_document(&bad).unwrap_err(),
            BlocksError::Malformed
        );
        assert_eq!(
            parse_template_document(&serde_json::json!({"channel": "email"})).unwrap_err(),
            BlocksError::Malformed
        );
    }

    #[test]
    fn a_document_that_fails_its_checks_does_not_convert() {
        let mut content = BlocksContent::new("Hi");
        content.settings.content_width = 10;
        let error = blocks_to_mjml(&content).unwrap_err();
        assert_eq!(error.issues.len(), 1);
        assert_eq!(error.issues[0].at, "/blocks/settings/content_width");
        assert!(error.to_string().contains('1'));
    }
}
