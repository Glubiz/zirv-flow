//! The `blocks` content type: the block document v1 and its renderer (architecture 11.1, 11.2,
//! 11.5, 11.6).
//!
//! A block email is a tree of sections, columns and blocks plus a few style settings. A render
//! goes through MiniJinja for each text field, MJML emitted from the tree, `mrml`, and a text
//! part built from the tree; [`blocks_to_mjml`] is the same walk without the first step, so a
//! marketer can leave the builder for MJML code with the merge tags intact.

mod check;
mod emit;
mod model;
mod render;
mod sanitize;
mod tags;

use std::fmt;

use super::{RenderKind, TemplateContent};
use crate::document::{Channel, Mode, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

pub(crate) use check::validate;
pub use model::{
    Align, Block, BlocksContent, BlocksError, Button, Divider, FONT_STACKS, Heading, Image,
    MAX_BLOCKS, MAX_COLUMNS, MAX_CONTENT_WIDTH, MAX_DOCUMENT_BYTES, MAX_ID_BYTES, MAX_IMAGE_WIDTH,
    MAX_LABEL_BYTES, MAX_RAW_HTML_BYTES, MAX_RICH_TEXT_BYTES, MAX_SECTIONS, MAX_SPACER_HEIGHT,
    MAX_SUBJECT_SOURCE_BYTES, MAX_URL_BYTES, MIN_CONTENT_WIDTH, PREHEADER_DISPLAY_CHARS, RawHtml,
    RichText, SUBJECT_DISPLAY_CHARS, Section, Settings, Spacer, parse_blocks_document,
};
pub(crate) use render::render;

pub(super) struct Blocks;

impl TemplateContent for Blocks {
    fn id(&self) -> &'static str {
        "blocks"
    }

    fn channel(&self) -> Channel {
        Channel::Email
    }

    fn handles(&self, doc: &TemplateDocument) -> bool {
        doc.channel == Channel::Email && doc.mode == Mode::Blocks && doc.blocks.is_some()
    }

    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
        validate(doc, catalog)
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
pub fn blocks_to_mjml(content: &BlocksContent) -> Result<Converted, ConvertError> {
    let issues: Vec<Issue> = check::check_sources(content)
        .into_iter()
        .filter(Issue::is_blocking)
        .collect();
    if !issues.is_empty() {
        return Err(ConvertError { issues });
    }
    match emit::emit(content, &mut emit::Sources) {
        Ok(emitted) => Ok(Converted {
            mjml: emitted.mjml,
            text: emitted.text,
        }),
        Err(never) => match never {},
    }
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
