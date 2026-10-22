//! Publish validation (architecture 11.6): everything that can be checked without a person.

use minijinja::Environment;
use zirv_flow_core::segment::pointer_push;

use crate::context::{MESSAGE_FIELDS, SYSTEM_FIELDS};
use crate::document::{
    Category, Channel, EmailContent, Mode, SCHEMA_VERSION, TemplateDocument, WebhookContent,
};
use crate::issue::{FieldCatalog, Issue, code};
use crate::links::link_urls;
use crate::render::has_template_syntax;
use crate::sandbox::{GLOBAL_FUNCTIONS, MAX_SOURCE_BYTES, environment};

/// The HTML size, in bytes, from which a warning says mail clients may clip the message. Gmail
/// clips at about 102 KB; this leaves room for the tracking rewrite and the footer.
pub(crate) const HTML_WARN_BYTES: usize = 90_000;

/// The merge path an unsubscribe link must use.
const UNSUBSCRIBE_PATH: &str = "system.unsubscribe_url";

/// Validate a document against the merge fields the workspace knows.
///
/// Blocking issues stop publishing; warnings do not (see [`Issue::is_blocking`]). The result is
/// in a fixed order, so equal inputs give equal issues.
pub fn validate(doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
    let mut issues = Vec::new();
    if doc.schema != SCHEMA_VERSION {
        issues.push(Issue::new(code::UNSUPPORTED_SCHEMA, "/schema"));
        return issues;
    }
    match doc.channel {
        Channel::Email | Channel::Webhook => {}
        Channel::Sms | Channel::Push | Channel::InApp => {
            issues.push(Issue::new(code::UNSUPPORTED_CHANNEL, "/channel"));
            return issues;
        }
    }
    if doc.mode != Mode::Code {
        issues.push(Issue::new(code::UNSUPPORTED_MODE, "/mode"));
        return issues;
    }
    let env = environment();
    match doc.channel {
        Channel::Email => match &doc.email {
            Some(email) => validate_email(&env, email, catalog, &mut issues),
            None => issues.push(Issue::new(code::MISSING_CONTENT, "/email")),
        },
        _ => match &doc.webhook {
            Some(webhook) => validate_webhook(&env, webhook, catalog, &mut issues),
            None => issues.push(Issue::new(code::MISSING_CONTENT, "/webhook")),
        },
    }
    issues
}

fn validate_email(
    env: &Environment<'static>,
    email: &EmailContent,
    catalog: &FieldCatalog,
    issues: &mut Vec<Issue>,
) {
    if email
        .sender
        .as_deref()
        .is_none_or(|sender| sender.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_SENDER, "/email/sender"));
    }
    if email
        .lang
        .as_deref()
        .is_none_or(|lang| lang.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_LANG, "/email/lang"));
    }
    check_template(env, catalog, "/email/subject", &email.subject, issues);
    let html_fields = check_template(env, catalog, "/email/html", &email.html, issues);
    check_template(env, catalog, "/email/text", &email.text, issues);

    if email.category == Category::Marketing
        && !html_fields.iter().any(|path| path == UNSUBSCRIBE_PATH)
    {
        issues.push(Issue::new(code::MISSING_UNSUBSCRIBE, "/email/html"));
    }
    if link_urls(&email.html).iter().any(|url| {
        url.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
    }) {
        issues.push(Issue::new(code::INSECURE_LINK, "/email/html"));
    }
    if email.html.len() >= HTML_WARN_BYTES {
        issues.push(Issue::new(code::HTML_NEAR_CLIP, "/email/html"));
    }
}

fn validate_webhook(
    env: &Environment<'static>,
    webhook: &WebhookContent,
    catalog: &FieldCatalog,
    issues: &mut Vec<Issue>,
) {
    for (name, source) in &webhook.headers {
        let at = pointer_push("/webhook/headers", name);
        check_template(env, catalog, &at, source, issues);
    }
    if webhook.body.len() > MAX_SOURCE_BYTES {
        issues.push(Issue::new(code::CONTENT_TOO_LARGE, "/webhook/body"));
        return;
    }
    match serde_json::from_str::<serde_json::Value>(&webhook.body) {
        Ok(body) => walk_body(env, catalog, "/webhook/body", &body, issues),
        Err(_) => issues.push(Issue::new(code::INVALID_JSON_BODY, "/webhook/body")),
    }
}

fn walk_body(
    env: &Environment<'static>,
    catalog: &FieldCatalog,
    at: &str,
    value: &serde_json::Value,
    issues: &mut Vec<Issue>,
) {
    match value {
        serde_json::Value::String(text) => {
            check_template(env, catalog, at, text, issues);
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk_body(env, catalog, &format!("{at}/{index}"), item, issues);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let child = pointer_push(at, key);
                if has_template_syntax(key) {
                    issues.push(Issue::new(code::TEMPLATE_IN_KEY, child.clone()));
                }
                walk_body(env, catalog, &child, item, issues);
            }
        }
        _ => {}
    }
}

/// Parse one template and check the merge fields it reads. Returns those field paths, sorted.
fn check_template(
    env: &Environment<'static>,
    catalog: &FieldCatalog,
    at: &str,
    source: &str,
    issues: &mut Vec<Issue>,
) -> Vec<String> {
    if source.len() > MAX_SOURCE_BYTES {
        issues.push(Issue::new(code::CONTENT_TOO_LARGE, at));
        return Vec::new();
    }
    let template = match env.template_from_named_str("check.txt", source) {
        Ok(template) => template,
        Err(_) => {
            issues.push(Issue::new(code::SYNTAX_ERROR, at));
            return Vec::new();
        }
    };
    let mut paths: Vec<String> = template.undeclared_variables(true).into_iter().collect();
    paths.sort();
    for path in &paths {
        if !is_known(catalog, path) {
            issues.push(Issue::new(code::UNKNOWN_MERGE_FIELD, at).with_field(path.clone()));
        }
    }
    paths
}

/// Whether a merge path (`person.first_name`) names something the template may read.
fn is_known(catalog: &FieldCatalog, path: &str) -> bool {
    let mut parts = path.split('.');
    let root = parts.next().unwrap_or_default();
    let second = parts.next();
    match (root, second) {
        ("person", Some(key)) => catalog.person.contains_key(key),
        ("event", Some(key)) => catalog.event.contains(key),
        ("message", Some(key)) => MESSAGE_FIELDS.contains(&key),
        ("system", Some(key)) => SYSTEM_FIELDS.contains(&key),
        ("person" | "event" | "message" | "system", None) => true,
        (name, _) => GLOBAL_FUNCTIONS.contains(&name),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zirv_flow_core::people::TraitType;

    use super::*;
    use crate::issue::Severity;

    fn catalog() -> FieldCatalog {
        FieldCatalog {
            person: BTreeMap::from([
                ("first_name".to_owned(), TraitType::String),
                ("age".to_owned(), TraitType::Number),
            ]),
            event: ["order_id".to_owned()].into_iter().collect(),
        }
    }

    fn good_email() -> EmailContent {
        EmailContent {
            subject: "Hi {{ person.first_name }}".into(),
            sender: Some("snd_1".into()),
            lang: Some("en".into()),
            category: Category::Marketing,
            html: r#"<p>{{ event.order_id }}</p><a href="{{ system.unsubscribe_url }}">u</a>"#
                .into(),
            text: "{{ person.age }} {{ message.id }} {{ system.preferences_url }}".into(),
        }
    }

    fn email_issues(change: impl FnOnce(&mut EmailContent)) -> Vec<Issue> {
        let mut email = good_email();
        change(&mut email);
        validate(&TemplateDocument::email_code(email), &catalog())
    }

    fn codes(issues: &[Issue]) -> Vec<&'static str> {
        issues.iter().map(|issue| issue.code).collect()
    }

    fn webhook_issues(headers: &[(&str, &str)], body: &str) -> Vec<Issue> {
        let doc = TemplateDocument::webhook_code(WebhookContent {
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.into(),
        });
        validate(&doc, &catalog())
    }

    #[test]
    fn a_good_email_has_no_issues() {
        assert_eq!(email_issues(|_| {}), Vec::new());
    }

    #[test]
    fn unsupported_schema() {
        let mut doc = TemplateDocument::email_code(good_email());
        doc.schema = 2;
        assert_eq!(
            codes(&validate(&doc, &catalog())),
            [code::UNSUPPORTED_SCHEMA]
        );
    }

    #[test]
    fn unsupported_channel() {
        let mut doc = TemplateDocument::email_code(good_email());
        doc.channel = Channel::Sms;
        let issues = validate(&doc, &catalog());
        assert_eq!(codes(&issues), [code::UNSUPPORTED_CHANNEL]);
        assert!(issues[0].is_blocking());
    }

    #[test]
    fn unsupported_mode() {
        let mut doc = TemplateDocument::email_code(good_email());
        doc.mode = Mode::Blocks;
        assert_eq!(codes(&validate(&doc, &catalog())), [code::UNSUPPORTED_MODE]);
    }

    #[test]
    fn missing_content() {
        let mut doc = TemplateDocument::email_code(good_email());
        doc.email = None;
        let issues = validate(&doc, &catalog());
        assert_eq!(codes(&issues), [code::MISSING_CONTENT]);
        assert_eq!(issues[0].at, "/email");
        let mut doc = TemplateDocument::webhook_code(WebhookContent::default());
        doc.webhook = None;
        assert_eq!(validate(&doc, &catalog())[0].at, "/webhook");
    }

    #[test]
    fn content_too_large() {
        let issues = email_issues(|email| email.text = "x".repeat(MAX_SOURCE_BYTES + 1));
        assert_eq!(codes(&issues), [code::CONTENT_TOO_LARGE]);
        assert_eq!(issues[0].at, "/email/text");
    }

    #[test]
    fn syntax_error() {
        let issues = email_issues(|email| email.subject = "{% if %}".into());
        assert_eq!(codes(&issues), [code::SYNTAX_ERROR]);
        assert_eq!(issues[0].at, "/email/subject");
    }

    #[test]
    fn unknown_merge_field() {
        let issues = email_issues(|email| {
            email.text =
                "{{ person.nickname }} {{ event.nope }} {{ other.x }} {{ system.nope }}".into();
        });
        assert_eq!(
            issues
                .iter()
                .map(|issue| issue.field.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["event.nope", "other.x", "person.nickname", "system.nope"]
        );
        assert!(
            issues
                .iter()
                .all(|issue| issue.code == code::UNKNOWN_MERGE_FIELD)
        );
        assert!(issues.iter().all(|issue| issue.at == "/email/text"));
    }

    #[test]
    fn loop_variables_and_globals_are_not_unknown_fields() {
        let issues = email_issues(|email| {
            email.text =
                "{% for tag in person.first_name %}{{ tag }}{% endfor %}{{ range(3) }}".into();
        });
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn missing_unsubscribe() {
        let issues = email_issues(|email| email.html = "<p>hi</p>".into());
        assert_eq!(codes(&issues), [code::MISSING_UNSUBSCRIBE]);
        assert!(issues[0].is_blocking());
        // Transactional mail does not need one.
        let issues = email_issues(|email| {
            email.html = "<p>hi</p>".into();
            email.category = Category::Transactional;
        });
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn missing_sender() {
        assert_eq!(
            codes(&email_issues(|email| email.sender = None)),
            [code::MISSING_SENDER]
        );
        assert_eq!(
            codes(&email_issues(|email| email.sender = Some("  ".into()))),
            [code::MISSING_SENDER]
        );
    }

    #[test]
    fn missing_lang_is_a_warning() {
        let issues = email_issues(|email| email.lang = None);
        assert_eq!(codes(&issues), [code::MISSING_LANG]);
        assert_eq!(issues[0].severity, Severity::Warning);
    }

    #[test]
    fn insecure_link_is_a_warning() {
        let issues = email_issues(|email| {
            email
                .html
                .push_str(r#"<a href="HTTP://shop.example/a">a</a>"#);
        });
        assert_eq!(codes(&issues), [code::INSECURE_LINK]);
        assert_eq!(issues[0].severity, Severity::Warning);
        let issues = email_issues(|email| {
            email
                .html
                .push_str(r#"<a href="https://shop.example/a">a</a>"#);
        });
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn html_near_clip_is_a_warning() {
        let issues = email_issues(|email| email.html.push_str(&" ".repeat(HTML_WARN_BYTES)));
        assert_eq!(codes(&issues), [code::HTML_NEAR_CLIP]);
        assert_eq!(issues[0].severity, Severity::Warning);
    }

    #[test]
    fn invalid_json_body() {
        let issues = webhook_issues(&[], r#"{"a": {{ person.age }}}"#);
        assert_eq!(codes(&issues), [code::INVALID_JSON_BODY]);
        assert_eq!(issues[0].at, "/webhook/body");
    }

    #[test]
    fn template_in_key() {
        let issues = webhook_issues(&[], r#"{"{{ person.age }}": 1}"#);
        assert_eq!(codes(&issues), [code::TEMPLATE_IN_KEY]);
        assert_eq!(issues[0].at, "/webhook/body/{{ person.age }}");
    }

    #[test]
    fn webhook_fields_are_checked_in_headers_and_body() {
        let issues = webhook_issues(
            &[("X-A", "{{ person.nope }}"), ("X/B", "{% if %}")],
            r#"{"list":["{{ event.order_id }}","{{ event.nope }}"],"n":1}"#,
        );
        assert_eq!(
            issues
                .iter()
                .map(|issue| (issue.code, issue.at.as_str()))
                .collect::<Vec<_>>(),
            [
                (code::UNKNOWN_MERGE_FIELD, "/webhook/headers/X-A"),
                (code::SYNTAX_ERROR, "/webhook/headers/X~1B"),
                (code::UNKNOWN_MERGE_FIELD, "/webhook/body/list/1"),
            ]
        );
    }

    #[test]
    fn webhook_body_size_is_checked() {
        let body = format!("\"{}\"", "x".repeat(MAX_SOURCE_BYTES));
        let issues = webhook_issues(&[], &body);
        assert_eq!(codes(&issues), [code::CONTENT_TOO_LARGE]);
    }

    #[test]
    fn a_good_webhook_has_no_issues() {
        let issues = webhook_issues(
            &[("X-A", "{{ person.first_name }}")],
            r#"{"id":"{{ message.id }}","age":"{{ person.age }}"}"#,
        );
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn every_code_is_produced_by_some_test() {
        // The tests above cover each code; this pins the list so a new code needs a new test.
        assert_eq!(code::BLOCKING.len() + code::WARNING.len(), 14);
    }
}
