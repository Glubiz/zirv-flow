//! The `html` content type: code-mode email, raw HTML and text with MiniJinja.

use super::{RenderKind, TemplateContent};
use crate::document::{Channel, Mode, Syntax, TemplateDocument};
use crate::issue::{FieldCatalog, Issue};

pub(super) struct Html;

impl TemplateContent for Html {
    fn id(&self) -> &'static str {
        "html"
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
                .is_none_or(|email| email.syntax == Syntax::Html)
    }

    fn validate(&self, doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
        crate::validate(doc, catalog)
    }

    fn render_kind(&self) -> RenderKind {
        RenderKind::Email
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{EmailContent, WebhookContent};

    #[test]
    fn it_owns_code_mode_email_only() {
        let email = TemplateDocument::email_code(EmailContent::default());
        assert!(Html.handles(&email));
        assert!(!Html.handles(&TemplateDocument::webhook_code(WebhookContent::default())));
        let mut blocks = email;
        blocks.mode = Mode::Blocks;
        assert!(!Html.handles(&blocks));
    }

    #[test]
    fn it_reports_the_missing_sender_of_an_empty_email() {
        let doc = TemplateDocument::email_code(EmailContent::default());
        let issues = Html.validate(&doc, &FieldCatalog::default());
        assert!(issues.iter().any(|issue| issue.code == "missing_sender"));
        assert_eq!(Html.render_kind(), RenderKind::Email);
    }
}
