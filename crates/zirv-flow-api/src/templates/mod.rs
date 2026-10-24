//! Message templates (architecture 11.6, 11.7 and 15): drafts, validation, publish into immutable
//! versions, where-used and preview.
//!
//! Permissions (ruling 43163028): `Read` reads, `Write` saves drafts and previews a sample
//! person, `Admin` publishes, archives and previews a real person (until the `view_pii` grant
//! exists). A caller with no access gets 404.
//!
//! Content types are carried opaquely: the store keeps the document as JSON, and validation and
//! the choice of render call go through the registry in `zirv-flow-templates`. Preview renders
//! only through the render pool in the application state, never in this process.

pub(crate) mod crud;
pub(crate) mod preview;
pub(crate) mod publish;
pub(crate) mod where_used;

use serde_json::Value;
use zirv_flow_store::{Template, TemplateError, TemplateFieldCatalog};
use zirv_flow_templates::{
    BlocksError, Category, Channel, EmailContent, FieldCatalog, Issue, Mode, Severity,
    TemplateDocument, WebhookContent, parse_template_document,
};

use crate::problem::{ApiError, TemplateIssueProblem};

/// The problem for a refused template write.
pub(crate) fn template_error(error: TemplateError) -> ApiError {
    match error {
        TemplateError::NotFound => ApiError::not_found(),
        TemplateError::Archived => ApiError::template_archived(),
        TemplateError::Store(error) => name_taken(error),
    }
}

/// A unique violation on a template write is a name that is taken.
pub(crate) fn name_taken(error: zirv_flow_store::StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a template with that name already exists",
        )
    } else {
        error.into()
    }
}

/// The renderer's field catalog from the store's read, plus the reserved `person.email`.
pub(crate) fn field_catalog(read: TemplateFieldCatalog) -> FieldCatalog {
    FieldCatalog {
        person: read.traits.into_iter().collect(),
        event: read.event_properties.into_iter().collect(),
    }
    .with_reserved_email()
}

/// The document as the renderer reads it, or the 422 that says it is not a template document. A
/// draft may be incomplete; it cannot be a document the schema does not parse, so the editor, the
/// preview and the publish read the same thing.
pub(crate) fn parse_document(value: &Value) -> Result<TemplateDocument, ApiError> {
    parse_template_document(value).map_err(|error| match error {
        BlocksError::UnsupportedSchema => ApiError::template_unsupported_schema(),
        BlocksError::UnknownBlock => ApiError::template_unknown_block(),
        BlocksError::UnknownField => ApiError::template_unknown_field(),
        BlocksError::AmbiguousContent => ApiError::template_malformed(
            "the document carries more than one of `email`, `blocks` and `webhook`",
        ),
        BlocksError::Malformed => {
            ApiError::template_malformed("the document is not a template document of a known shape")
        }
    })
}

/// A stored document. It was parsed when it was saved, but a stricter envelope can refuse a row
/// that an older one took, so it is read the way a draft is and fails with the same 422 code, not
/// a 500.
pub(crate) fn stored_document(value: &Value) -> Result<TemplateDocument, ApiError> {
    parse_document(value)
}

/// The name of a channel as the template row spells it.
pub(super) fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Email => "email",
        Channel::Webhook => "webhook",
        Channel::Sms => "sms",
        Channel::Push => "push",
        Channel::InApp => "in_app",
    }
}

/// The name of a category as the template row spells it.
pub(super) fn category_name(category: Category) -> &'static str {
    match category {
        Category::Marketing => "marketing",
        Category::Transactional => "transactional",
    }
}

/// Whether `doc` belongs to `template`: the channel and the category are fixed when the template
/// is made, so a document cannot move it to another one.
pub(crate) fn check_matches(template: &Template, doc: &TemplateDocument) -> Result<(), ApiError> {
    if channel_name(doc.channel) != template.channel {
        return Err(ApiError::template_mismatch(format!(
            "the document is for the {} channel but the template is {}",
            channel_name(doc.channel),
            template.channel
        )));
    }
    // The draft carries its own channel's content and no other's: a webhook body on an email
    // template would be stored, published and then ignored.
    let (own, other) = match doc.channel {
        Channel::Webhook => (doc.webhook.is_some(), doc.email.is_some()),
        _ => (doc.email.is_some(), doc.webhook.is_some()),
    };
    if other {
        return Err(ApiError::template_mismatch(format!(
            "the document carries content for another channel than the template's {}",
            template.channel
        )));
    }
    if !own && doc.mode == Mode::Code {
        return Err(ApiError::template_malformed(format!(
            "a code-mode {} document needs its {} content",
            template.channel, template.channel
        )));
    }
    let category = doc
        .email
        .as_ref()
        .map(|email| email.category)
        .or_else(|| doc.blocks.as_ref().map(|blocks| blocks.category));
    if let Some(category) = category
        && category_name(category) != template.category
    {
        return Err(ApiError::template_mismatch(format!(
            "the document is {} but the template is {}",
            category_name(category),
            template.category
        )));
    }
    Ok(())
}

/// The empty document a new template starts with.
pub(crate) fn skeleton(channel: Channel, category: Category) -> TemplateDocument {
    match channel {
        Channel::Webhook => TemplateDocument::webhook_code(WebhookContent {
            body: "{}".to_owned(),
            ..WebhookContent::default()
        }),
        _ => TemplateDocument::email_code(EmailContent {
            category,
            ..EmailContent::default()
        }),
    }
}

/// The issues as the problem document and the response bodies carry them.
pub(crate) fn issue_problems(issues: &[Issue]) -> Vec<TemplateIssueProblem> {
    issues
        .iter()
        .map(|issue| TemplateIssueProblem {
            code: issue.code.to_owned(),
            severity: match issue.severity {
                Severity::Blocking => "blocking",
                Severity::Warning => "warning",
            }
            .to_owned(),
            at: issue.at.clone(),
            field: issue.field.clone(),
            message: issue.message.clone(),
        })
        .collect()
}

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn template(channel: &str, category: &str) -> Template {
        Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: channel.into(),
            category: category.into(),
            sender_identity_id: None,
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_catalog_always_knows_the_reserved_email_as_a_string() {
        let mut read = TemplateFieldCatalog::default();
        read.traits.insert("plan".into(), TraitType::String);
        read.traits.insert("email".into(), TraitType::Number);

        let catalog = field_catalog(read);

        assert_eq!(catalog.person["plan"], TraitType::String);
        assert_eq!(catalog.person["email"], TraitType::String);
        assert!(
            field_catalog(TemplateFieldCatalog::default())
                .person
                .contains_key("email")
        );
    }

    #[test]
    fn a_document_must_match_its_templates_channel_and_category() {
        let marketing = skeleton(Channel::Email, Category::Marketing);
        let transactional = skeleton(Channel::Email, Category::Transactional);
        let webhook = skeleton(Channel::Webhook, Category::Marketing);

        assert!(check_matches(&template("email", "marketing"), &marketing).is_ok());
        assert!(check_matches(&template("webhook", "transactional"), &webhook).is_ok());
        let wrong_category = check_matches(&template("email", "marketing"), &transactional);
        let wrong_channel = check_matches(&template("email", "marketing"), &webhook);
        assert_eq!(
            wrong_category.unwrap_err().code(),
            Some("template_mismatch")
        );
        assert_eq!(wrong_channel.unwrap_err().code(), Some("template_mismatch"));
    }

    #[test]
    fn a_draft_carries_its_own_channels_content_and_no_other() {
        let email_template = template("email", "marketing");
        let webhook_template = template("webhook", "transactional");
        let mut email_with_hook = skeleton(Channel::Email, Category::Marketing);
        email_with_hook.webhook = skeleton(Channel::Webhook, Category::Marketing).webhook;
        let mut hook_with_email = skeleton(Channel::Webhook, Category::Marketing);
        hook_with_email.email = skeleton(Channel::Email, Category::Marketing).email;
        let mut email_without_content = skeleton(Channel::Email, Category::Marketing);
        email_without_content.email = None;
        let mut hook_without_content = skeleton(Channel::Webhook, Category::Marketing);
        hook_without_content.webhook = None;

        let code = |result: Result<(), ApiError>| result.unwrap_err().code();

        assert_eq!(
            code(check_matches(&email_template, &email_with_hook)),
            Some("template_mismatch")
        );
        assert_eq!(
            code(check_matches(&webhook_template, &hook_with_email)),
            Some("template_mismatch")
        );
        assert_eq!(
            code(check_matches(&email_template, &email_without_content)),
            Some("template_malformed")
        );
        assert_eq!(
            code(check_matches(&webhook_template, &hook_without_content)),
            Some("template_malformed")
        );
    }

    #[test]
    fn a_document_that_is_not_a_template_document_is_malformed() {
        assert!(parse_document(&json!({"schema": 1, "channel": "email", "mode": "code"})).is_ok());
        for bad in [
            json!("text"),
            json!({"channel": "email"}),
            json!({"schema": 1, "channel": "fax", "mode": "code"}),
        ] {
            assert_eq!(
                parse_document(&bad).unwrap_err().code(),
                Some("template_malformed")
            );
        }
    }

    #[test]
    fn a_skeleton_matches_its_own_channel_and_category() {
        for (channel, name) in [(Channel::Email, "email"), (Channel::Webhook, "webhook")] {
            let doc = skeleton(channel, Category::Transactional);
            assert_eq!(channel_name(doc.channel), name);
        }
        let email = skeleton(Channel::Email, Category::Transactional);
        assert_eq!(
            email.email.map(|content| content.category),
            Some(Category::Transactional)
        );
    }

    #[test]
    fn issues_keep_their_code_severity_and_place() {
        let doc = skeleton(Channel::Email, Category::Marketing);
        let issues = zirv_flow_templates::validate(&doc, &FieldCatalog::default());
        let problems = issue_problems(&issues);

        assert_eq!(problems.len(), issues.len());
        let sender = problems
            .iter()
            .find(|issue| issue.code == "missing_sender")
            .unwrap();
        assert_eq!(
            (sender.severity.as_str(), sender.at.as_str()),
            ("blocking", "/email/sender")
        );
        let lang = problems
            .iter()
            .find(|issue| issue.code == "missing_lang")
            .unwrap();
        assert_eq!(lang.severity, "warning");
    }
}
