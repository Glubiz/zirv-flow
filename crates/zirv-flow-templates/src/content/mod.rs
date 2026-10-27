//! The template content-type registry (architecture 5.3): the one place that lists the content
//! types, and the one trait each of them implements.
//!
//! A content type is one module in this directory and one line in [`REGISTRY`]. Validation and
//! the choice of render call dispatch through it, so the store and the API carry a document's
//! content type opaquely and a new type changes neither. A document of an unregistered type is
//! refused with [`ContentError::UnsupportedDocument`], a name with [`ContentError::Unregistered`].

pub(crate) mod blocks;
mod html;
pub(crate) mod mjml;
mod webhook;

use std::fmt;

use crate::context::RenderExtras;
use crate::document::{Channel, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

/// Guard known conditional-comment panics before tokenizing and catch unknown tokenizer panics.
fn with_tokenizer<'a, R>(
    text: &'a str,
    read: impl FnOnce(htmlparser::Tokenizer<'a>) -> R,
) -> Option<R> {
    for prefix in ["<![if", "<!--[if", "<![endif"] {
        let mut close = 0;
        for (at, _) in text.match_indices(prefix) {
            let end = at + prefix.len();
            let rest = &text[end..];
            if prefix == "<![endif" && !rest.starts_with(']') {
                return None;
            }
            if close < end {
                close = end + rest.find(']')?;
            }
            let suffix = &text[close + 1..];
            if !suffix.starts_with('>') && !suffix.starts_with("-->") {
                return None;
            }
        }
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read(htmlparser::Tokenizer::from(text))
    }))
    .ok()
}

/// Which render call a content type's documents go through. The render host serves one request
/// kind for each (`RenderRequest`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderKind {
    Email,
    Webhook,
}

/// One kind of template content.
pub trait TemplateContent: Sync {
    /// The stable name of the type, such as `html`.
    fn id(&self) -> &'static str;

    /// The channel its documents are written for.
    fn channel(&self) -> Channel;

    /// Whether this type owns `doc` (by channel and mode).
    fn handles(&self, doc: &TemplateDocument) -> bool;

    /// The publish checks for a document this type handles.
    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue>;

    /// [`validate`](Self::validate) with the resolved render inputs. A type that reads none of
    /// them keeps this default.
    fn validate_with(
        &self,
        doc: &TemplateDocument,
        catalog: &FieldCatalog,
        _extras: &RenderExtras,
    ) -> Vec<Issue> {
        self.validate(doc, catalog)
    }

    /// The render call the type's documents use.
    fn render_kind(&self) -> RenderKind;
}

/// Every registered content type. Adding one is a module and a line here.
const REGISTRY: [&dyn TemplateContent; 4] =
    [&html::Html, &mjml::Mjml, &blocks::Blocks, &webhook::Webhook];

/// A document or a name that no registered content type owns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentError {
    /// No registered type has this name.
    Unregistered(String),
    /// No registered type owns a document of this channel and mode.
    UnsupportedDocument,
}

impl fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unregistered(id) => write!(formatter, "no content type is registered as {id}"),
            Self::UnsupportedDocument => {
                formatter.write_str("no registered content type handles this document")
            }
        }
    }
}

impl std::error::Error for ContentError {}

/// The names of the registered content types, in registry order.
pub fn content_type_ids() -> Vec<&'static str> {
    REGISTRY.iter().map(|content| content.id()).collect()
}

/// The content type registered under `id`.
pub fn content_type(id: &str) -> Result<&'static dyn TemplateContent, ContentError> {
    REGISTRY
        .iter()
        .copied()
        .find(|content| content.id() == id)
        .ok_or_else(|| ContentError::Unregistered(id.to_owned()))
}

/// The content type that owns `doc`.
pub fn content_type_of(
    doc: &TemplateDocument,
) -> Result<&'static dyn TemplateContent, ContentError> {
    REGISTRY
        .iter()
        .copied()
        .find(|content| content.handles(doc))
        .ok_or(ContentError::UnsupportedDocument)
}

/// Validate `doc` through its content type. A document no registered type owns is refused rather
/// than checked as something else.
pub fn validate_document(
    doc: &TemplateDocument,
    catalog: &FieldCatalog,
) -> Result<Vec<Issue>, ContentError> {
    validate_in(&REGISTRY, doc, catalog)
}

/// [`validate_document`] with the resolved render inputs.
pub fn validate_document_with(
    doc: &TemplateDocument,
    catalog: &FieldCatalog,
    extras: &RenderExtras,
) -> Result<Vec<Issue>, ContentError> {
    REGISTRY
        .iter()
        .find(|content| content.handles(doc))
        .map(|content| content.validate_with(doc, catalog, extras))
        .ok_or(ContentError::UnsupportedDocument)
}

/// [`validate_document`] over a given set of types, so a test can register one of its own.
fn validate_in(
    registry: &[&'static dyn TemplateContent],
    doc: &TemplateDocument,
    catalog: &FieldCatalog,
) -> Result<Vec<Issue>, ContentError> {
    registry
        .iter()
        .find(|content| content.handles(doc))
        .map(|content| content.validate(doc, catalog))
        .ok_or(ContentError::UnsupportedDocument)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{EmailContent, Mode, WebhookContent};

    #[test]
    fn the_guard_rejects_the_fuzz_crash_before_reading_tokens() {
        let document: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../tests/fixtures/fuzz/template_document-crash-861764cf.bin"
        ))
        .unwrap();
        let content = document["blocks"]["sections"][0]["columns"][0][1]["content"]
            .as_str()
            .unwrap();
        // Without iteration, catch_unwind cannot mask a missing guard.
        assert_eq!(with_tokenizer(content, |_| ()), None);
    }

    #[test]
    fn the_guard_handles_many_conditional_prefixes() {
        let mut input = "<![if".repeat(50_000);
        assert_eq!(with_tokenizer(&input, |_| ()), None);
        input.push_str("]>");
        assert_eq!(with_tokenizer(&input, |_| ()), Some(()));
    }

    /// A type of its own, to show that the type that owns a document is the one whose check runs.
    struct Marker;

    impl TemplateContent for Marker {
        fn id(&self) -> &'static str {
            "marker"
        }
        fn channel(&self) -> Channel {
            Channel::Email
        }
        fn handles(&self, doc: &TemplateDocument) -> bool {
            doc.mode == Mode::Blocks
        }
        fn validate(&self, _doc: &TemplateDocument, _catalog: &FieldCatalog) -> Vec<Issue> {
            vec![Issue::new(crate::issue::code::MISSING_LANG, "/marker")]
        }
        fn render_kind(&self) -> RenderKind {
            RenderKind::Email
        }
    }

    #[test]
    fn the_check_that_runs_is_the_one_of_the_type_that_owns_the_document() {
        let mut blocks = TemplateDocument::email_code(EmailContent::default());
        blocks.mode = Mode::Blocks;
        let registry: [&'static dyn TemplateContent; 4] =
            [&html::Html, &mjml::Mjml, &webhook::Webhook, &Marker];

        let issues = validate_in(&registry, &blocks, &FieldCatalog::default()).unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].at, "/marker");
        // Without that type registered, nothing owns the document.
        assert_eq!(
            validate_in(&registry[..3], &blocks, &FieldCatalog::default()).err(),
            Some(ContentError::UnsupportedDocument)
        );
    }

    #[test]
    fn html_mjml_blocks_and_webhook_are_the_only_registered_types() {
        assert_eq!(content_type_ids(), ["html", "mjml", "blocks", "webhook"]);
        assert_eq!(content_type("html").unwrap().channel(), Channel::Email);
        assert_eq!(
            content_type("webhook").unwrap().render_kind(),
            RenderKind::Webhook
        );
    }

    #[test]
    fn an_unregistered_name_is_a_typed_error() {
        for id in ["snippet", "", "HTML", "MJML", "Blocks"] {
            assert_eq!(
                content_type(id).err(),
                Some(ContentError::Unregistered(id.to_owned())),
                "{id}"
            );
        }
    }

    #[test]
    fn a_document_is_owned_by_its_channel_and_mode() {
        let email = TemplateDocument::email_code(EmailContent::default());
        let webhook = TemplateDocument::webhook_code(WebhookContent::default());
        assert_eq!(content_type_of(&email).unwrap().id(), "html");
        assert_eq!(content_type_of(&webhook).unwrap().id(), "webhook");

        let mut blocks = email.clone();
        blocks.mode = Mode::Blocks;
        assert_eq!(
            content_type_of(&blocks).err(),
            Some(ContentError::UnsupportedDocument)
        );
        let mut sms = email;
        sms.channel = Channel::Sms;
        assert_eq!(
            validate_document(&sms, &FieldCatalog::default()).err(),
            Some(ContentError::UnsupportedDocument)
        );
    }

    #[test]
    fn validation_through_the_registry_equals_the_direct_call() {
        let doc = TemplateDocument::email_code(EmailContent::default());
        let catalog = FieldCatalog::default();
        assert_eq!(
            validate_document(&doc, &catalog).unwrap(),
            crate::validate(&doc, &catalog)
        );
    }

    #[test]
    fn every_registered_type_has_a_distinct_name() {
        let mut ids = content_type_ids();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), REGISTRY.len());
    }
}
