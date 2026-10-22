//! The template document: a versioned, channel-aware envelope (architecture 11.1).
//!
//! M6 renders `mode: code` for the `email` and `webhook` channels. `blocks` mode and the
//! reserved channels are part of the schema so documents keep their shape, but they are
//! validated as unsupported and never rendered here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The document schema this crate reads. A newer schema is refused; older ones are migrated in
/// M7 (architecture 11.8).
pub const SCHEMA_VERSION: u32 = 1;

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
    /// The block builder (M7).
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

/// A template document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TemplateDocument {
    pub schema: u32,
    pub channel: Channel,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<EmailContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookContent>,
}

impl TemplateDocument {
    /// A code-mode email document at the current schema.
    pub fn email_code(content: EmailContent) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            channel: Channel::Email,
            mode: Mode::Code,
            email: Some(content),
            webhook: None,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_document_reads_with_defaults() {
        let doc: TemplateDocument = serde_json::from_str(
            r#"{"schema":1,"channel":"email","mode":"code",
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
    fn reserved_channels_parse() {
        let doc: TemplateDocument =
            serde_json::from_str(r#"{"schema":1,"channel":"in_app","mode":"blocks"}"#).unwrap();
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
