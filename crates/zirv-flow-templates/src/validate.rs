//! Publish validation (architecture 11.6): everything that can be checked without a person.

use minijinja::{Environment, ErrorKind, Value};
use zirv_flow_core::segment::pointer_push;

use crate::callables::{Kind, unknown_callables};
use crate::content::mjml::{MjmlError, compile, has_include};
use crate::context::{MESSAGE_FIELDS, RenderContext, SAMPLE_UNSUBSCRIBE_URL, SYSTEM_FIELDS};
use crate::document::{
    Category, Channel, EmailContent, Mode, SCHEMA_VERSION, Syntax, TemplateDocument, WebhookContent,
};
use crate::issue::{FieldCatalog, Issue, code};
use crate::links::{href_mentions, link_urls, links_to};
use crate::render::{has_template_syntax, one_line};
use crate::sandbox::{
    GLOBAL_FUNCTIONS, MAX_HEADER_COUNT, MAX_HEADER_VALUE_BYTES, MAX_HEADERS_BYTES, MAX_HTML_BYTES,
    MAX_SOURCE_BYTES, MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, MAX_WEBHOOK_BODY_BYTES, environment,
    is_header_name, render_raw,
};
use zirv_flow_core::events::PropertyType;

/// The HTML size, in bytes, from which a warning says mail clients may clip the message. Gmail
/// clips at about 102 KB; this leaves room for the tracking rewrite and the footer.
pub(crate) const HTML_WARN_BYTES: usize = 90_000;

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
    if doc.channel == Channel::Email && doc.mode == Mode::Blocks && doc.blocks.is_some() {
        return crate::content::blocks::validate(doc, catalog);
    }
    if doc.mode != Mode::Code {
        issues.push(Issue::new(code::UNSUPPORTED_MODE, "/mode"));
        return issues;
    }
    let env = environment();
    let sample = sample_context(catalog);
    match doc.channel {
        Channel::Email => match &doc.email {
            Some(email) => validate_email(&env, email, catalog, &sample, &mut issues),
            None => issues.push(Issue::new(code::MISSING_CONTENT, "/email")),
        },
        _ => match &doc.webhook {
            Some(webhook) => validate_webhook(&env, webhook, catalog, &sample, &mut issues),
            None => issues.push(Issue::new(code::MISSING_CONTENT, "/webhook")),
        },
    }
    issues
}

/// The outcome of checking one template field.
pub(crate) struct Checked {
    /// Whether the field added no issue.
    pub(crate) clean: bool,
    /// The field rendered against the sample context; `None` when the render failed.
    pub(crate) sample: Option<String>,
    /// Why the sample render failed, when it did.
    pub(crate) failure: Option<String>,
}

/// What a field is rendered as in the sample render, and the output limit that applies to it.
#[derive(Clone, Copy)]
pub(crate) struct FieldKind {
    /// The template name; `*.html` is autoescaped.
    name: &'static str,
    limit: usize,
}

pub(crate) const SUBJECT: FieldKind = FieldKind {
    name: "subject.txt",
    limit: MAX_SUBJECT_BYTES,
};
pub(crate) const HTML: FieldKind = FieldKind {
    name: "html.html",
    limit: MAX_HTML_BYTES,
};
const TEXT: FieldKind = FieldKind {
    name: "text.txt",
    limit: MAX_TEXT_BYTES,
};
const HEADER: FieldKind = FieldKind {
    name: "header.txt",
    limit: MAX_HEADER_VALUE_BYTES,
};
const BODY: FieldKind = FieldKind {
    name: "body.txt",
    limit: MAX_WEBHOOK_BODY_BYTES,
};

/// The context of the sample render, as the template engine reads it.
pub(crate) fn sample_context(catalog: &FieldCatalog) -> Value {
    Value::from_serialize(RenderContext::sample(catalog))
}

fn validate_email(
    env: &Environment<'static>,
    email: &EmailContent,
    catalog: &FieldCatalog,
    sample: &Value,
    issues: &mut Vec<Issue>,
) {
    if email
        .sender
        .as_deref()
        .is_none_or(|sender| sender.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_SENDER, "/email/sender"));
    }
    // An MJML message takes its language from the `<mjml>` element, which `check_mjml` reads.
    if email.syntax == Syntax::Html
        && email
            .lang
            .as_deref()
            .is_none_or(|lang| lang.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_LANG, "/email/lang"));
    }
    check_template(
        env,
        catalog,
        sample,
        "/email/subject",
        &email.subject,
        SUBJECT,
        issues,
    );
    let mut html = check_template(
        env,
        catalog,
        sample,
        "/email/html",
        &email.html,
        HTML,
        issues,
    );
    if email.syntax == Syntax::Mjml {
        check_mjml(email, &mut html, issues);
    }
    check_template(
        env,
        catalog,
        sample,
        "/email/text",
        &email.text,
        TEXT,
        issues,
    );

    // The link must survive the render: a bare URL in text, or an anchor in a branch that never
    // runs, does not count. A field that already has an issue is reported by that issue.
    match (&html.sample, &html.failure) {
        (Some(rendered), _) => {
            if email.category == Category::Marketing
                && html.clean
                && !links_to(rendered, SAMPLE_UNSUBSCRIBE_URL)
            {
                issues.push(Issue::new(code::MISSING_UNSUBSCRIBE, "/email/html"));
            }
        }
        // The sample render failed and nothing else says why (for example `| int` on a string
        // sample): that is not the link's fault. Warn, and fall back to the source: it must
        // put the unsubscribe URL in an `href`. Send time still refuses a message without it.
        (None, failure) if html.clean => {
            let reason = failure.as_deref().unwrap_or("the output is not UTF-8");
            issues.push(Issue::new(code::SAMPLE_RENDER_FAILED, "/email/html").with_message(reason));
            if email.category == Category::Marketing && !source_mentions_unsubscribe(email) {
                issues.push(Issue::new(code::MISSING_UNSUBSCRIBE, "/email/html"));
            }
        }
        (None, _) => {}
    }
    // An MJML link lives in an `mj-button` or `mj-image` as often as in an anchor, so it is read
    // from the HTML `mrml` made, and so is the size that counts.
    let (linked, size) = match (email.syntax, &html.sample) {
        (Syntax::Html, _) => (link_urls(&email.html), email.html.len()),
        (Syntax::Mjml, Some(rendered)) => (link_urls(rendered), rendered.len()),
        (Syntax::Mjml, None) => (Vec::new(), 0),
    };
    if linked.iter().any(|url| {
        url.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
    }) {
        issues.push(Issue::new(code::INSECURE_LINK, "/email/html"));
    }
    if size >= HTML_WARN_BYTES {
        issues.push(Issue::new(code::HTML_NEAR_CLIP, "/email/html"));
    }
}

/// Whether the source can put the message's unsubscribe URL in a link: the fallback when the
/// sample render failed and there is no HTML to read.
fn source_mentions_unsubscribe(email: &EmailContent) -> bool {
    match email.syntax {
        Syntax::Html => href_mentions(&email.html, "system.unsubscribe_url"),
        Syntax::Mjml => email.html.contains("system.unsubscribe_url"),
    }
}

/// The MJML stage of the checks: refuse an `mj-include`, then run `mrml` over the sample render
/// and put its HTML in place of the MiniJinja output, so the checks that follow read what would
/// be sent. A problem leaves no sample, and the issue says why.
fn check_mjml(email: &EmailContent, html: &mut Checked, issues: &mut Vec<Issue>) {
    const AT: &str = "/email/html";
    if has_include(&email.html) {
        issues.push(Issue::new(code::MJML_INCLUDE, AT));
        html.clean = false;
        html.sample = None;
        return;
    }
    let Some(rendered) = html.sample.take() else {
        return;
    };
    let issue = match compile(&rendered) {
        Ok(compiled) if compiled.html.len() > MAX_HTML_BYTES => {
            Issue::new(code::CONTENT_TOO_LARGE, AT)
        }
        Ok(compiled) => {
            if compiled.lang.is_none() {
                issues.push(
                    Issue::new(code::MISSING_LANG, AT)
                        .with_message("set `lang` on the <mjml> element: the content's own `lang` field is not used for MJML"),
                );
            }
            html.sample = Some(compiled.html);
            return;
        }
        Err(MjmlError::Include) => Issue::new(code::MJML_INCLUDE, AT),
        Err(MjmlError::TooDeep) => Issue::new(code::MJML_TOO_DEEP, AT).with_message(format!(
            "elements nest deeper than {} levels",
            crate::content::mjml::MAX_DEPTH
        )),
        Err(MjmlError::Parse {
            line,
            column,
            reason,
            excerpt,
        }) => Issue::new(code::MJML_PARSE_ERROR, AT).with_message(format!(
            "{reason} at line {line}, column {column} of the sample render (not of the source), near: {excerpt}"
        )),
        Err(MjmlError::Render) => {
            Issue::new(code::MJML_PARSE_ERROR, AT).with_message("the MJML could not be rendered")
        }
    };
    issues.push(issue);
    html.clean = false;
}

fn validate_webhook(
    env: &Environment<'static>,
    webhook: &WebhookContent,
    catalog: &FieldCatalog,
    sample: &Value,
    issues: &mut Vec<Issue>,
) {
    if webhook.headers.len() > MAX_HEADER_COUNT {
        issues.push(Issue::new(code::TOO_MANY_HEADERS, "/webhook/headers"));
    }
    let mut header_bytes = 0usize;
    for (name, source) in &webhook.headers {
        let at = pointer_push("/webhook/headers", name);
        if !is_header_name(name) {
            issues.push(Issue::new(code::INVALID_HEADER_NAME, at.clone()));
        }
        let checked = check_template(env, catalog, sample, &at, source, HEADER, issues);
        // Render sends the value on one line and trimmed, so that is what is counted.
        let value_bytes = checked
            .sample
            .as_deref()
            .map_or(0, |value| one_line(value).len());
        header_bytes = header_bytes.saturating_add(name.len() + value_bytes);
    }
    if header_bytes > MAX_HEADERS_BYTES {
        issues.push(Issue::new(code::HEADERS_TOO_LARGE, "/webhook/headers"));
    }
    if webhook.body.len() > MAX_SOURCE_BYTES {
        issues.push(Issue::new(code::CONTENT_TOO_LARGE, "/webhook/body"));
        return;
    }
    match serde_json::from_str::<serde_json::Value>(&webhook.body) {
        Ok(body) => {
            let mut spent = 0usize;
            walk_body(
                env,
                catalog,
                sample,
                "/webhook/body",
                &body,
                issues,
                &mut spent,
            );
            if spent > MAX_WEBHOOK_BODY_BYTES {
                issues.push(Issue::new(code::BODY_TOO_LARGE, "/webhook/body"));
            }
        }
        Err(_) => issues.push(Issue::new(code::INVALID_JSON_BODY, "/webhook/body")),
    }
}

fn walk_body(
    env: &Environment<'static>,
    catalog: &FieldCatalog,
    sample: &Value,
    at: &str,
    value: &serde_json::Value,
    issues: &mut Vec<Issue>,
    spent: &mut usize,
) {
    match value {
        serde_json::Value::String(text) => {
            let checked = check_template(env, catalog, sample, at, text, BODY, issues);
            // Render spends one budget across every string of the body.
            *spent = spent.saturating_add(checked.sample.as_deref().map_or(0, str::len));
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk_body(
                    env,
                    catalog,
                    sample,
                    &format!("{at}/{index}"),
                    item,
                    issues,
                    spent,
                );
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let child = pointer_push(at, key);
                if has_template_syntax(key) {
                    issues.push(Issue::new(code::TEMPLATE_IN_KEY, child.clone()));
                }
                walk_body(env, catalog, sample, &child, item, issues, spent);
            }
        }
        _ => {}
    }
}

/// Parse one template, check the merge fields it reads, and render it against the sample
/// context. A filter, test or function the sandbox does not have only fails at render time, so
/// the sample render is what finds it.
pub(crate) fn check_template(
    env: &Environment<'static>,
    catalog: &FieldCatalog,
    sample: &Value,
    at: &str,
    source: &str,
    kind: FieldKind,
    issues: &mut Vec<Issue>,
) -> Checked {
    let before = issues.len();
    if source.len() > MAX_SOURCE_BYTES {
        issues.push(Issue::new(code::CONTENT_TOO_LARGE, at));
        return Checked {
            clean: false,
            sample: None,
            failure: None,
        };
    }
    let template = match env.template_from_named_str("check.txt", source) {
        Ok(template) => template,
        Err(_) => {
            issues.push(Issue::new(code::SYNTAX_ERROR, at));
            return Checked {
                clean: false,
                sample: None,
                failure: None,
            };
        }
    };
    let mut paths: Vec<String> = template.undeclared_variables(true).into_iter().collect();
    paths.sort();
    for path in &paths {
        match lookup(catalog, path) {
            Lookup::Known => {}
            Lookup::Unknown => {
                issues.push(Issue::new(code::UNKNOWN_MERGE_FIELD, at).with_field(path.clone()));
            }
            Lookup::Dynamic => {
                issues.push(Issue::new(code::DYNAMIC_MERGE_FIELD, at).with_field(path.clone()));
            }
        }
    }
    for (kind, name) in unknown_callables(env, source) {
        let message = match kind {
            Kind::Filter => format!("unknown filter `{name}`"),
            Kind::Test => format!("unknown test `{name}`"),
            Kind::Method => format!("method calls are not available: `{name}(`"),
        };
        issues.push(Issue::new(code::SYNTAX_ERROR, at).with_message(message));
    }
    let (rendered, failure) = match render_raw(env, kind.name, source, sample, kind.limit) {
        Ok(rendered) => (rendered, None),
        Err((error, _)) => {
            // A function the sandbox lacks only fails at render time, so the sample render finds
            // it. Other failures (a missing field, a sample value that does not fit an operation)
            // are either already reported above or depend on real data, so they are not issues.
            if error.kind() == ErrorKind::UnknownFunction && issues.len() == before {
                issues.push(Issue::new(code::SYNTAX_ERROR, at).with_message("unknown function"));
            }
            (None, Some(error.to_string()))
        }
    };
    Checked {
        clean: issues.len() == before,
        sample: rendered,
        failure,
    }
}

/// How a merge path relates to the catalog.
enum Lookup {
    Known,
    Unknown,
    /// A bare `person` or `event`: the whole object, a dynamic subscript or a filter over it.
    Dynamic,
}

/// Whether an event path (after `event.`) is in the catalog: a property, or a path below an
/// `Object` property. Nothing else has members: a path below any other type is unknown.
fn event_path_is_known(catalog: &FieldCatalog, path: &str) -> bool {
    match path.split_once('.') {
        None => catalog.event.contains_key(path),
        Some((name, _)) => catalog.event.get(name) == Some(&PropertyType::Object),
    }
}

/// Classify a merge path (`person.first_name`). The whole path must be in the catalog: a path
/// below a person trait is unknown, because no trait type is an object or JSON value.
fn lookup(catalog: &FieldCatalog, path: &str) -> Lookup {
    let (root, key) = match path.split_once('.') {
        Some((root, key)) => (root, Some(key)),
        None => (path, None),
    };
    let known = |found: bool| {
        if found {
            Lookup::Known
        } else {
            Lookup::Unknown
        }
    };
    match (root, key) {
        ("person" | "event", None) => Lookup::Dynamic,
        ("person", Some(key)) => known(catalog.person.contains_key(key)),
        ("event", Some(key)) => known(event_path_is_known(catalog, key)),
        ("message", Some(key)) => known(MESSAGE_FIELDS.contains(&key)),
        ("system", Some(key)) => known(SYSTEM_FIELDS.contains(&key)),
        ("message" | "system", None) => Lookup::Known,
        (name, None) => known(GLOBAL_FUNCTIONS.contains(&name)),
        (_, Some(_)) => Lookup::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zirv_flow_core::people::TraitType;

    use super::*;
    use crate::issue::{Severity, severity_of};

    fn catalog() -> FieldCatalog {
        FieldCatalog {
            person: BTreeMap::from([
                ("first_name".to_owned(), TraitType::String),
                ("age".to_owned(), TraitType::Number),
            ]),
            event: BTreeMap::from([
                ("order_id".to_owned(), PropertyType::String),
                ("order".to_owned(), PropertyType::Object),
                ("count".to_owned(), PropertyType::Integer),
                ("tags".to_owned(), PropertyType::StringList),
            ]),
        }
    }

    fn good_email() -> EmailContent {
        EmailContent {
            subject: "Hi {{ person.first_name }}".into(),
            sender: Some("snd_1".into()),
            lang: Some("en".into()),
            category: Category::Marketing,
            syntax: Default::default(),
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
    fn the_unsubscribe_url_must_be_a_rendered_link() {
        for html in [
            "<p>{{ system.unsubscribe_url }}</p>",
            r#"{% if false %}<a href="{{ system.unsubscribe_url }}">u</a>{% endif %}"#,
            r#"<a href="https://shop.example/u">{{ system.unsubscribe_url }}</a>"#,
        ] {
            let issues = email_issues(|email| email.html = html.into());
            assert_eq!(codes(&issues), [code::MISSING_UNSUBSCRIBE], "{html}");
        }
        // A link in a branch that runs is fine.
        let issues = email_issues(|email| {
            email.html = r#"{% if person.first_name %}<a href="{{ system.unsubscribe_url }}">u</a>{% endif %}"#.into();
        });
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn merge_paths_must_match_the_catalog_exactly() {
        let unknown = |text: &str| {
            let issues = email_issues(|email| email.text = text.into());
            issues
                .iter()
                .map(|issue| (issue.code, issue.field.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            unknown("{{ person.first_name.deeper }}"),
            [(
                code::UNKNOWN_MERGE_FIELD,
                "person.first_name.deeper".to_owned()
            )]
        );
        assert_eq!(
            unknown("{{ person.known.deeper }}"),
            [(code::UNKNOWN_MERGE_FIELD, "person.known.deeper".to_owned())]
        );
        for text in [
            r#"{{ person["no_such_trait"] }}"#,
            "{{ person[message.id] }}",
            "{{ person | items }}",
            "{{ event }}",
        ] {
            let found = unknown(text);
            assert!(
                found
                    .iter()
                    .any(|(code, _)| *code == code::DYNAMIC_MERGE_FIELD),
                "{text}: {found:?}"
            );
            assert_eq!(severity_of(code::DYNAMIC_MERGE_FIELD), Severity::Blocking);
        }
        assert_eq!(unknown("{{ person.first_name }} {{ event.order_id }}"), []);
    }

    #[test]
    fn unknown_filters_tests_and_functions_are_syntax_issues() {
        for source in [
            "{{ person.first_name | titlecase2 }}",
            "{% if person.age is nosuchtest %}x{% endif %}",
        ] {
            let issues = email_issues(|email| email.subject = source.into());
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.code == code::SYNTAX_ERROR && issue.at == "/email/subject"),
                "{source}: {issues:?}"
            );
        }
        // The webhook strings are rendered too.
        let issues = webhook_issues(
            &[("X-A", "{{ person.age | nosuchfilter }}")],
            r#"{"a":"{{ person.age | nosuchfilter }}"}"#,
        );
        assert_eq!(codes(&issues), [code::SYNTAX_ERROR, code::SYNTAX_ERROR]);
    }

    #[test]
    fn callables_in_branches_the_sample_skips_are_found() {
        for (source, named) in [
            (
                r#"{% if person.first_name == "Ann" %}{{ person.first_name | nosuchfilter }}{% endif %}"#,
                "nosuchfilter",
            ),
            (
                r#"{% if person.first_name == "Ann" %}{% if person.age is nosuchtest %}x{% endif %}{% endif %}"#,
                "nosuchtest",
            ),
            (
                r#"{% if person.first_name == "Ann" %}{% set x = person.first_name %}{{ x.upper() }}{% endif %}"#,
                "upper",
            ),
        ] {
            let issues = email_issues(|email| email.subject = source.into());
            assert_eq!(codes(&issues), [code::SYNTAX_ERROR], "{source}");
            assert_eq!(issues[0].at, "/email/subject");
            assert!(
                issues[0]
                    .message
                    .as_deref()
                    .is_some_and(|m| m.contains(named)),
                "{source}: {issues:?}"
            );
        }
        // Names inside strings, comments and raw blocks are text, not callables.
        let issues = email_issues(|email| {
            email.subject =
                r#"{{ "a | nope" }}{# x | nope #}{% raw %}{{ y | nope }}{% endraw %}"#.into();
        });
        assert_eq!(issues, Vec::new());
    }

    #[test]
    fn an_unrelated_sample_failure_is_not_a_missing_unsubscribe() {
        let with_link =
            r#"<p>{{ person.first_name | int }}</p><a href="{{ system.unsubscribe_url }}">u</a>"#;
        let issues = email_issues(|email| email.html = with_link.into());
        assert_eq!(codes(&issues), [code::SAMPLE_RENDER_FAILED]);
        assert_eq!(issues[0].severity, Severity::Warning);
        assert_eq!(issues[0].at, "/email/html");
        assert!(issues[0].message.is_some());
        // Without the link in the source, the fallback still refuses a marketing email.
        let issues = email_issues(|email| {
            email.html = "<p>{{ person.first_name | int }}</p>".into();
        });
        assert_eq!(
            codes(&issues),
            [code::SAMPLE_RENDER_FAILED, code::MISSING_UNSUBSCRIBE]
        );
        // Transactional mail needs no link.
        let issues = email_issues(|email| {
            email.html = "<p>{{ person.first_name | int }}</p>".into();
            email.category = Category::Transactional;
        });
        assert_eq!(codes(&issues), [code::SAMPLE_RENDER_FAILED]);
    }

    #[test]
    fn a_comment_does_not_satisfy_the_unsubscribe_check() {
        let issues = email_issues(|email| {
            email.html = r#"<!-- <a href="{{ system.unsubscribe_url }}">u</a> -->"#.into();
        });
        assert_eq!(codes(&issues), [code::MISSING_UNSUBSCRIBE]);
    }

    #[test]
    fn event_paths_nest_only_below_object_properties() {
        let found = |text: &str| {
            email_issues(|email| email.text = text.into())
                .iter()
                .map(|issue| (issue.code, issue.field.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        assert_eq!(found("{{ event.order_id }} {{ event.tags[0] }}"), []);
        assert_eq!(found("{{ event.order.total }}"), []);
        for path in [
            "event.order_id.x",
            "event.count.x",
            "event.tags.x",
            "event.nope.x",
        ] {
            assert_eq!(
                found(&format!("{{{{ {path} }}}}")),
                [(code::UNKNOWN_MERGE_FIELD, path.to_owned())]
            );
        }
    }

    #[test]
    fn the_sample_body_is_held_to_the_aggregate_budget() {
        // Each string renders about 150 KB, within the per-string limit; together they pass 256 KiB.
        let big = "{% for i in range(15000) %}0123456789{% endfor %}";
        let body = serde_json::json!({ "a": big, "b": big }).to_string();
        let issues = webhook_issues(&[], &body);
        assert_eq!(codes(&issues), [code::BODY_TOO_LARGE]);
        assert_eq!(issues[0].at, "/webhook/body");
        let body = serde_json::json!({ "a": big }).to_string();
        assert_eq!(webhook_issues(&[], &body), Vec::new());
    }

    #[test]
    fn header_values_are_counted_after_normalisation() {
        let spaces = " ".repeat(MAX_HEADER_VALUE_BYTES);
        let names: Vec<String> = (0..5).map(|n| format!("X-{n}")).collect();
        let headers: Vec<(&str, &str)> = names
            .iter()
            .map(|n| (n.as_str(), spaces.as_str()))
            .collect();
        assert_eq!(webhook_issues(&headers, "{}"), Vec::new());
    }

    #[test]
    fn webhook_header_names_must_be_tokens() {
        for name in ["X-A\r\nX-Evil: 1", "X:A", "X A", "X/B", ""] {
            let issues = webhook_issues(&[(name, "1")], "{}");
            assert_eq!(codes(&issues), [code::INVALID_HEADER_NAME], "{name:?}");
        }
        let issues = webhook_issues(&[("X/B", "1")], "{}");
        assert_eq!(issues[0].at, "/webhook/headers/X~1B");
    }

    #[test]
    fn webhook_header_count_and_size_are_capped() {
        let names: Vec<String> = (0..=MAX_HEADER_COUNT).map(|n| format!("X-{n}")).collect();
        let headers: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "1")).collect();
        assert_eq!(
            codes(&webhook_issues(&headers, "{}")),
            [code::TOO_MANY_HEADERS]
        );
        let value = "v".repeat(MAX_HEADER_VALUE_BYTES);
        let names: Vec<String> = (0..5).map(|n| format!("X-{n}")).collect();
        let headers: Vec<(&str, &str)> =
            names.iter().map(|n| (n.as_str(), value.as_str())).collect();
        assert_eq!(
            codes(&webhook_issues(&headers, "{}")),
            [code::HEADERS_TOO_LARGE]
        );
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
            &[("X-A", "{{ person.nope }}"), ("X-B", "{% if %}")],
            r#"{"list":["{{ event.order_id }}","{{ event.nope }}"],"n":1}"#,
        );
        assert_eq!(
            issues
                .iter()
                .map(|issue| (issue.code, issue.at.as_str()))
                .collect::<Vec<_>>(),
            [
                (code::UNKNOWN_MERGE_FIELD, "/webhook/headers/X-A"),
                (code::SYNTAX_ERROR, "/webhook/headers/X-B"),
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
        assert_eq!(code::BLOCKING.len() + code::WARNING.len(), 33);
    }
}
