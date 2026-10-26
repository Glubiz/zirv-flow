//! The template document: a versioned, channel-aware envelope (architecture 11.1).
//!
//! M6 renders `mode: code` for the `email` and `webhook` channels, and `mode: blocks` for `email`
//! (the block document, in `content::blocks`). The reserved channels are part of the schema so
//! documents keep their shape, but they are validated as unsupported and never rendered here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::content::blocks::BlocksContent;
pub use crate::reserved::{InAppContent, PushContent, SmsContent};

/// The document schema this crate reads. A newer schema is refused; older ones are migrated in
/// memory when they are read (architecture 11.8, [`crate::migrate_to_current`]).
pub const SCHEMA_VERSION: u32 = 2;

/// The channel a document is written for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Webhook,
    /// Reserved: defined from day one, not publishable.
    Sms,
    /// Reserved.
    Push,
    /// Reserved.
    InApp,
}

/// How the content is authored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The block document: sections of blocks, rendered to MJML.
    Blocks,
    /// Raw HTML and text, or headers and a JSON body, with MiniJinja.
    Code,
}

/// Whether a message needs the unsubscribe link and headers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// The default, because it is the stricter one.
    #[default]
    Marketing,
    Transactional,
}

/// How the `html` field of an email is written.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Syntax {
    /// Raw HTML. The default, so every document written before `syntax` existed is unchanged.
    #[default]
    Html,
    /// MJML: MiniJinja renders the source, then `mrml` turns the result into responsive HTML.
    Mjml,
}

impl Syntax {
    pub(crate) fn is_html(&self) -> bool {
        *self == Self::Html
    }
}

/// Code-mode email content. Every text field is a MiniJinja template.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmailContent {
    pub subject: String,
    /// The sender identity id; resolved to an address by the send pipeline, never rendered.
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub category: Category,
    /// How `html` is written. Left out of the stored document when it is `html`.
    #[serde(default, skip_serializing_if = "Syntax::is_html")]
    pub syntax: Syntax,
    pub html: String,
    #[serde(default)]
    pub text: String,
}

/// Webhook content: header templates and a JSON body template.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebhookContent {
    /// Header name to a MiniJinja template for the value.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON text. MiniJinja is allowed only inside string values, so the text must parse as
    /// JSON as written: that is what keeps the structure valid after rendering.
    pub body: String,
}

/// A template document. It carries at most one content (`email`, `blocks` or `webhook`); a
/// document with two is [`ambiguous`](Self::is_ambiguous) and no content type owns it, so no
/// category, sender or other field can come from the wrong one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateDocument {
    pub schema: u32,
    pub channel: Channel,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<EmailContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookContent>,
    /// The content of a `mode: blocks` email. It sits beside the code content so a code document
    /// reads and writes exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<BlocksContent>,
    /// Reserved channel contents: validated, never publishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sms: Option<SmsContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_app: Option<InAppContent>,
}

impl TemplateDocument {
    /// How many contents (`email`, `blocks`, `webhook`, `sms`, `push`, `in_app`) the document
    /// carries.
    pub fn content_count(&self) -> usize {
        usize::from(self.email.is_some())
            + usize::from(self.blocks.is_some())
            + usize::from(self.webhook.is_some())
            + usize::from(self.sms.is_some())
            + usize::from(self.push.is_some())
            + usize::from(self.in_app.is_some())
    }

    /// Whether the document carries more than one content.
    pub fn is_ambiguous(&self) -> bool {
        self.content_count() > 1
    }

    /// A code-mode email document at the current schema.
    pub fn email_code(content: EmailContent) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            channel: Channel::Email,
            mode: Mode::Code,
            email: Some(content),
            webhook: None,
            blocks: None,
            sms: None,
            push: None,
            in_app: None,
        }
    }

    /// A block-mode email document at the current schema.
    pub fn email_blocks(content: BlocksContent) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            channel: Channel::Email,
            mode: Mode::Blocks,
            email: None,
            webhook: None,
            blocks: Some(content),
            sms: None,
            push: None,
            in_app: None,
        }
    }

    /// A code-mode webhook document at the current schema.
    pub fn webhook_code(content: WebhookContent) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            channel: Channel::Webhook,
            mode: Mode::Code,
            email: None,
            webhook: Some(content),
            blocks: None,
            sms: None,
            push: None,
            in_app: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_document_reads_with_defaults() {
        let doc: TemplateDocument = serde_json::from_str(
            r#"{"schema":2,"channel":"email","mode":"code",
                "email":{"subject":"Hi","html":"<p>x</p>"}}"#,
        )
        .unwrap();
        let email = doc.email.unwrap();
        assert_eq!(email.category, Category::Marketing);
        assert_eq!(email.text, "");
        assert_eq!(email.sender, None);
        assert_eq!(doc.channel, Channel::Email);
    }

    #[test]
    fn an_unknown_syntax_is_refused_with_a_serde_error() {
        let error = serde_json::from_str::<TemplateDocument>(
            r#"{"schema":2,"channel":"email","mode":"code",
                "email":{"subject":"Hi","html":"x","syntax":"markdown"}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown variant `markdown`"), "{error}");
        assert!(
            error.contains("`html`") && error.contains("`mjml`"),
            "{error}"
        );
    }

    #[test]
    fn reserved_channels_parse() {
        let doc: TemplateDocument =
            serde_json::from_str(r#"{"schema":2,"channel":"in_app","mode":"blocks"}"#).unwrap();
        assert_eq!(doc.channel, Channel::InApp);
        assert_eq!(doc.mode, Mode::Blocks);
    }

    #[test]
    fn document_round_trips() {
        let doc = TemplateDocument::webhook_code(WebhookContent {
            headers: BTreeMap::from([("X-A".to_owned(), "1".to_owned())]),
            body: "{}".to_owned(),
        });
        let json = serde_json::to_string(&doc).unwrap();
        assert_eq!(
            serde_json::from_str::<TemplateDocument>(&json).unwrap(),
            doc
        );
    }
}
