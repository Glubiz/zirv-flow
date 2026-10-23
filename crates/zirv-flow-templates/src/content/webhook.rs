//! The `webhook` content type: header templates and a JSON body template.

use super::{RenderKind, TemplateContent};
use crate::document::{Channel, Mode, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

pub(super) struct Webhook;

impl TemplateContent for Webhook {
    fn id(&self) -> &'static str {
        "webhook"
    }

    fn channel(&self) -> Channel {
        Channel::Webhook
    }

    fn handles(&self, doc: &TemplateDocument) -> bool {
        !doc.is_ambiguous() && doc.channel == Channel::Webhook && doc.mode == Mode::Code
    }

    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
        crate::validate(doc, catalog)
    }

    fn render_kind(&self) -> RenderKind {
        RenderKind::Webhook
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::people::TraitType;

    use super::*;
    use crate::document::{EmailContent, WebhookContent};

    #[test]
    fn it_owns_code_mode_webhooks_only() {
        let webhook = TemplateDocument::webhook_code(WebhookContent::default());
        assert!(Webhook.handles(&webhook));
        assert!(!Webhook.handles(&TemplateDocument::email_code(EmailContent::default())));
        assert_eq!(Webhook.render_kind(), RenderKind::Webhook);
    }

    #[test]
    fn it_accepts_a_valid_body_and_refuses_a_broken_one() {
        let ok = TemplateDocument::webhook_code(WebhookContent {
            body: r#"{"a":"{{ person.x }}"}"#.into(),
            ..WebhookContent::default()
        });
        let mut catalog = FieldCatalog::default();
        catalog.person.insert("x".into(), TraitType::String);
        assert!(Webhook.validate(&ok, &catalog).is_empty());
        let broken = TemplateDocument::webhook_code(WebhookContent {
            body: "{not json".into(),
            ..WebhookContent::default()
        });
        let issues = Webhook.validate(&broken, &catalog);
        assert!(issues.iter().any(|issue| issue.code == "invalid_json_body"));
    }
}
