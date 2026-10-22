//! Rendering: code-mode email and webhook content through the sandbox (architecture 11.5, 11.7).

use minijinja::{Environment, Value};
use serde::Serialize;

use crate::context::RenderContext;
use crate::document::{
    Category, Channel, EmailContent, Mode, SCHEMA_VERSION, TemplateDocument, WebhookContent,
};
use crate::error::RenderError;
use crate::links::{Hooks, links_to, rewrite_links, unsubscribe_headers};
use crate::sandbox::{
    MAX_HEADER_COUNT, MAX_HEADER_VALUE_BYTES, MAX_HEADERS_BYTES, MAX_HTML_BYTES, MAX_SOURCE_BYTES,
    MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, MAX_WEBHOOK_BODY_BYTES, environment, is_header_name,
    map_error, render_string,
};

/// A rendered email.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Rendered {
    /// One line: line breaks in the rendered subject become spaces.
    pub subject: String,
    pub html: String,
    pub text: String,
    /// Extra message headers, in a fixed order: the `List-Unsubscribe` pair for marketing mail.
    pub headers: Vec<(String, String)>,
}

/// A rendered webhook payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderedWebhook {
    /// Header name to value, in name order.
    pub headers: Vec<(String, String)>,
    /// Compact JSON text.
    pub body: String,
}

fn email_of(doc: &TemplateDocument) -> Result<&EmailContent, RenderError> {
    if doc.schema != SCHEMA_VERSION {
        return Err(RenderError::UnsupportedSchema);
    }
    match (&doc.email, doc.channel, doc.mode) {
        (Some(email), Channel::Email, Mode::Code) => Ok(email),
        _ => Err(RenderError::WrongDocument),
    }
}

fn webhook_of(doc: &TemplateDocument) -> Result<&WebhookContent, RenderError> {
    if doc.schema != SCHEMA_VERSION {
        return Err(RenderError::UnsupportedSchema);
    }
    match (&doc.webhook, doc.channel, doc.mode) {
        (Some(webhook), Channel::Webhook, Mode::Code) => Ok(webhook),
        _ => Err(RenderError::WrongDocument),
    }
}

/// Replace line breaks and other control characters with a space, so a rendered value can never
/// start a new header line.
pub(crate) fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Render a code-mode email: subject, HTML and text through the sandbox, then the post-render
/// steps. Marketing mail must link to the message's unsubscribe URL and gets the
/// `List-Unsubscribe` headers.
pub fn render_email(
    doc: &TemplateDocument,
    context: &RenderContext,
    hooks: &Hooks<'_>,
) -> Result<Rendered, RenderError> {
    let email = email_of(doc)?;
    let env = environment();
    let values = Value::from_serialize(context);
    let subject = one_line(&render_string(
        &env,
        "subject.txt",
        "subject",
        &email.subject,
        &values,
        MAX_SUBJECT_BYTES,
    )?);
    let html = render_string(
        &env,
        "html.html",
        "html",
        &email.html,
        &values,
        MAX_HTML_BYTES,
    )?;
    let text = render_string(
        &env,
        "text.txt",
        "text",
        &email.text,
        &values,
        MAX_TEXT_BYTES,
    )?;

    let system = &context.system;
    let headers = match email.category {
        Category::Marketing => {
            let headers = unsubscribe_headers(&system.unsubscribe_url)?;
            if !links_to(&html, &system.unsubscribe_url) {
                return Err(RenderError::MissingUnsubscribe);
            }
            headers
        }
        Category::Transactional => Vec::new(),
    };
    let keep = [
        system.unsubscribe_url.as_str(),
        system.preferences_url.as_str(),
    ];
    let html = rewrite_links(&html, &hooks.tracking, &keep);
    // Rewriting lengthens each link, so the cap that held for the render is checked again.
    if html.len() > MAX_HTML_BYTES {
        return Err(RenderError::OutputTooLarge { field: "html" });
    }
    Ok(Rendered {
        subject,
        html,
        text,
        headers,
    })
}

/// Render webhook content: the header values and the JSON body.
///
/// The body is parsed as JSON first and only its string values are rendered, so the result is
/// valid JSON whatever the person data holds. A string that is exactly one merge tag
/// (`"{{ person.age }}"`) is replaced by the value itself, keeping its type (number, boolean,
/// list). Object keys are never rendered.
pub fn render_webhook(
    doc: &TemplateDocument,
    context: &RenderContext,
) -> Result<RenderedWebhook, RenderError> {
    let webhook = webhook_of(doc)?;
    if webhook.body.len() > MAX_SOURCE_BYTES {
        return Err(RenderError::SourceTooLarge { field: "body" });
    }
    let template: serde_json::Value =
        serde_json::from_str(&webhook.body).map_err(|_| RenderError::InvalidBody)?;
    let env = environment();
    let values = Value::from_serialize(context);

    if webhook.headers.len() > MAX_HEADER_COUNT {
        return Err(RenderError::TooManyHeaders);
    }
    if webhook.headers.keys().any(|name| !is_header_name(name)) {
        return Err(RenderError::InvalidHeaderName);
    }
    let mut header_bytes = 0usize;
    let mut headers = Vec::with_capacity(webhook.headers.len());
    for (name, source) in &webhook.headers {
        let value = render_string(
            &env,
            "header.txt",
            "headers",
            source,
            &values,
            MAX_HEADER_VALUE_BYTES,
        )?;
        let value = one_line(&value);
        header_bytes = header_bytes.saturating_add(name.len() + value.len());
        if header_bytes > MAX_HEADERS_BYTES {
            return Err(RenderError::HeadersTooLarge);
        }
        headers.push((name.clone(), value));
    }

    let mut body = BodyRenderer {
        env: &env,
        values: &values,
        budget: MAX_WEBHOOK_BODY_BYTES,
    };
    let rendered = body.value(template)?;
    let body = serde_json::to_string(&rendered).map_err(|_| RenderError::InvalidBody)?;
    if body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(RenderError::BodyTooLarge);
    }
    Ok(RenderedWebhook { headers, body })
}

/// Whether a string holds MiniJinja syntax.
pub(crate) fn has_template_syntax(text: &str) -> bool {
    text.contains("{{") || text.contains("{%") || text.contains("{#")
}

/// The expression of a string that is exactly one merge tag, `{{ expression }}`.
pub(crate) fn single_tag(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    let plain = !has_template_syntax(inner)
        && !inner.contains("}}")
        && !inner.starts_with(['-', '+'])
        && !inner.ends_with(['-', '+'])
        && !inner.trim().is_empty();
    plain.then_some(inner)
}

struct BodyRenderer<'a> {
    env: &'a Environment<'static>,
    values: &'a Value,
    /// Output bytes still allowed, across the whole body.
    budget: usize,
}

impl BodyRenderer<'_> {
    fn value(&mut self, value: serde_json::Value) -> Result<serde_json::Value, RenderError> {
        use serde_json::Value as Json;
        Ok(match value {
            Json::String(text) => self.string(text)?,
            Json::Array(items) => Json::Array(
                items
                    .into_iter()
                    .map(|item| self.value(item))
                    .collect::<Result<_, _>>()?,
            ),
            Json::Object(map) => {
                let mut out = serde_json::Map::new();
                for (key, item) in map {
                    if has_template_syntax(&key) {
                        return Err(RenderError::TemplateInKey);
                    }
                    out.insert(key, self.value(item)?);
                }
                Json::Object(out)
            }
            other => other,
        })
    }

    fn string(&mut self, text: String) -> Result<serde_json::Value, RenderError> {
        use serde_json::Value as Json;
        if let Some(expression) = single_tag(&text) {
            let compiled = self
                .env
                .compile_expression(expression)
                .map_err(|error| map_error(&error, "body", false))?;
            let value = compiled
                .eval(self.values)
                .map_err(|error| map_error(&error, "body", false))?;
            if value.is_undefined() {
                return Err(RenderError::Undefined { field: "body" });
            }
            let json = serde_json::to_value(&value)
                .map_err(|_| RenderError::Template { field: "body" })?;
            self.spend(json.to_string().len())?;
            return Ok(json);
        }
        if !has_template_syntax(&text) {
            self.spend(text.len())?;
            return Ok(Json::String(text));
        }
        let rendered = render_string(
            self.env,
            "body.txt",
            "body",
            &text,
            self.values,
            self.budget,
        )
        .map_err(|error| match error {
            RenderError::OutputTooLarge { .. } => RenderError::BodyTooLarge,
            other => other,
        })?;
        self.spend(rendered.len())?;
        Ok(Json::String(rendered))
    }

    fn spend(&mut self, bytes: usize) -> Result<(), RenderError> {
        self.budget = self
            .budget
            .checked_sub(bytes)
            .ok_or(RenderError::BodyTooLarge)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use serde_json::json;

    use super::*;
    use crate::links::Tracking;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn tok(index: usize, _url: &str) -> String {
        format!("tok{index}")
    }

    fn live<'a>() -> Hooks<'a> {
        Hooks {
            tracking: Tracking::Rewrite {
                base_url: "https://t.example/c",
                token: &tok,
            },
        }
    }

    fn email(category: Category, html: &str) -> TemplateDocument {
        TemplateDocument::email_code(EmailContent {
            subject: "Hi {{ person.name }}".into(),
            sender: Some("snd_1".into()),
            lang: Some("en".into()),
            category,
            html: html.into(),
            text: "Hello {{ person.name }}".into(),
        })
    }

    fn context() -> RenderContext {
        let mut context = RenderContext::default();
        context.person.insert("name".into(), json!("Ann <b>"));
        context.person.insert("age".into(), json!(41));
        context.person.insert("vip".into(), json!(true));
        context.system.unsubscribe_url = "https://u.example/u/1?s=a&t=b".into();
        context.system.preferences_url = "https://u.example/p/1".into();
        context
    }

    const FOOTER: &str = r#"<a href="{{ system.unsubscribe_url }}">Unsubscribe</a>"#;

    fn webhook(headers: &[(&str, &str)], body: &str) -> TemplateDocument {
        TemplateDocument::webhook_code(WebhookContent {
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            body: body.into(),
        })
    }

    // ----- golden corpus -------------------------------------------------------------------

    fn check(path: &PathBuf, actual: &str) {
        if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
            fs::write(path, actual).unwrap();
        }
        let expected = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("missing {}; bless with ZIRV_FLOW_BLESS=1", path.display()));
        assert_eq!(expected.replace("\r\n", "\n"), actual, "{}", path.display());
    }

    fn golden_email(name: &str) {
        let dir = fixtures();
        let read = |suffix: &str| fs::read_to_string(dir.join(format!("{name}.{suffix}"))).unwrap();
        let doc: TemplateDocument = serde_json::from_str(&read("doc.json")).unwrap();
        let context: RenderContext = serde_json::from_str(&read("context.json")).unwrap();
        let rendered = render_email(&doc, &context, &live()).unwrap();
        check(&dir.join(format!("{name}.expected.html")), &rendered.html);
        check(&dir.join(format!("{name}.expected.txt")), &rendered.text);
        let meta = json!({ "subject": rendered.subject, "headers": rendered.headers });
        check(
            &dir.join(format!("{name}.expected.json")),
            &(serde_json::to_string_pretty(&meta).unwrap() + "\n"),
        );
    }

    fn golden_webhook(name: &str) {
        let dir = fixtures();
        let read = |suffix: &str| fs::read_to_string(dir.join(format!("{name}.{suffix}"))).unwrap();
        let doc: TemplateDocument = serde_json::from_str(&read("doc.json")).unwrap();
        let context: RenderContext = serde_json::from_str(&read("context.json")).unwrap();
        let rendered = render_webhook(&doc, &context).unwrap();
        check(
            &dir.join(format!("{name}.expected.json")),
            &(serde_json::to_string_pretty(&rendered).unwrap() + "\n"),
        );
    }

    #[test]
    fn golden_marketing_email() {
        golden_email("email_marketing");
    }

    #[test]
    fn golden_transactional_email() {
        golden_email("email_transactional");
    }

    #[test]
    fn golden_webhook_typed_values() {
        golden_webhook("webhook_order");
    }

    // ----- email ---------------------------------------------------------------------------

    #[test]
    fn email_escapes_html_only() {
        let doc = email(
            Category::Marketing,
            &format!("<p>{{{{ person.name }}}}</p>{FOOTER}"),
        );
        let out = render_email(&doc, &context(), &Hooks::untracked()).unwrap();
        assert!(out.html.starts_with("<p>Ann &lt;b&gt;</p>"), "{}", out.html);
        assert_eq!(out.text, "Hello Ann <b>");
        assert_eq!(out.subject, "Hi Ann <b>");
    }

    #[test]
    fn subject_is_one_line() {
        let mut context = context();
        context
            .person
            .insert("name".into(), json!("a\r\nBcc: x@y.example"));
        let doc = email(Category::Transactional, "<p>x</p>");
        let out = render_email(&doc, &context, &Hooks::untracked()).unwrap();
        assert_eq!(out.subject, "Hi a  Bcc: x@y.example");
    }

    #[test]
    fn marketing_gets_the_one_click_headers() {
        let doc = email(Category::Marketing, FOOTER);
        let out = render_email(&doc, &context(), &live()).unwrap();
        assert_eq!(
            out.headers,
            vec![
                (
                    "List-Unsubscribe".to_owned(),
                    "<https://u.example/u/1?s=a&t=b>".to_owned()
                ),
                (
                    "List-Unsubscribe-Post".to_owned(),
                    "List-Unsubscribe=One-Click".to_owned()
                ),
            ]
        );
        // The unsubscribe link itself is not click-tracked (the sandbox escapes its slashes).
        assert!(links_to(&out.html, "https://u.example/u/1?s=a&t=b"));
        assert!(!out.html.contains("t.example"));
    }

    #[test]
    fn transactional_has_no_unsubscribe_requirement() {
        let doc = email(
            Category::Transactional,
            r#"<a href="https://shop.example/o/1">Order</a>"#,
        );
        let out = render_email(&doc, &context(), &live()).unwrap();
        assert!(out.headers.is_empty());
        assert!(out.html.contains("https://t.example/c/tok0"));
    }

    #[test]
    fn marketing_without_the_unsubscribe_link_is_refused() {
        let doc = email(Category::Marketing, "<p>no footer</p>");
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::MissingUnsubscribe)
        );
    }

    #[test]
    fn marketing_with_a_bad_unsubscribe_url_is_refused() {
        let mut context = context();
        context.system.unsubscribe_url = "http://u.example/u/1".into();
        let doc = email(Category::Marketing, FOOTER);
        assert_eq!(
            render_email(&doc, &context, &live()),
            Err(RenderError::InvalidUnsubscribeUrl)
        );
    }

    #[test]
    fn inert_preview_has_the_structure_of_a_send() {
        let doc = email(
            Category::Marketing,
            &format!(r#"<a href="https://shop.example/a">A</a>{FOOTER}"#),
        );
        let preview = render_email(
            &doc,
            &context(),
            &Hooks {
                tracking: Tracking::Inert {
                    base_url: "https://t.example/c",
                },
            },
        )
        .unwrap();
        let send = render_email(&doc, &context(), &live()).unwrap();
        assert_eq!(preview.subject, send.subject);
        assert_eq!(preview.text, send.text);
        assert_eq!(preview.headers, send.headers);
        assert!(preview.html.contains("https://t.example/c/inert"));
        assert_eq!(
            preview.html.replace("inert", "tok0"),
            send.html,
            "only the token differs"
        );
    }

    #[test]
    fn wrong_documents_are_refused() {
        let mut doc = email(Category::Marketing, FOOTER);
        doc.schema = 2;
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::UnsupportedSchema)
        );
        let mut doc = email(Category::Marketing, FOOTER);
        doc.mode = Mode::Blocks;
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::WrongDocument)
        );
        let doc = webhook(&[], "{}");
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::WrongDocument)
        );
        assert_eq!(
            render_webhook(&email(Category::Marketing, FOOTER), &context()),
            Err(RenderError::WrongDocument)
        );
    }

    #[test]
    fn email_sandbox_errors_name_the_field() {
        let doc = email(Category::Transactional, "{{ person.nope }}");
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::Undefined { field: "html" })
        );
        let doc = email(
            Category::Transactional,
            "{% for a in range(1000) %}{% for b in range(1000) %}x{% endfor %}{% endfor %}",
        );
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::OutOfFuel { field: "html" })
        );
        let doc = email(Category::Transactional, "{{ 'x' * 3 }}{% if %}");
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::Syntax { field: "html" })
        );
    }

    #[test]
    fn oversized_html_is_refused() {
        // 10 characters, repeated by a loop, past the HTML limit but within fuel.
        let doc = email(
            Category::Transactional,
            &format!(
                "{{% for i in range(60) %}}{}{{% endfor %}}",
                "x".repeat(10_000)
            ),
        );
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::OutputTooLarge { field: "html" })
        );
    }

    #[test]
    fn link_rewriting_cannot_push_the_html_past_its_cap() {
        // Each bare link is 8 bytes and becomes 27: under the cap as rendered, over it after.
        let html = format!(
            "{{% for i in range(20000) %}}<a href=http://a>{{% endfor %}}{}",
            "x".repeat(100)
        );
        let doc = email(Category::Transactional, &html);
        let untracked = render_email(&doc, &context(), &Hooks::untracked()).unwrap();
        assert!(untracked.html.len() <= MAX_HTML_BYTES);
        assert_eq!(
            render_email(&doc, &context(), &live()),
            Err(RenderError::OutputTooLarge { field: "html" })
        );
    }

    // ----- webhook -------------------------------------------------------------------------

    #[test]
    fn exact_merge_tags_keep_their_type() {
        let doc = webhook(
            &[],
            r#"{"age":"{{ person.age }}","vip":"{{ person.vip }}","n":"{{ person.age + 1 }}","s":"Hi {{ person.name }}","k":"plain","list":["{{ person.age }}"],"nil":null}"#,
        );
        let out = render_webhook(&doc, &context()).unwrap();
        assert_eq!(
            out.body,
            r#"{"age":41,"k":"plain","list":[41],"n":42,"nil":null,"s":"Hi Ann <b>","vip":true}"#
        );
    }

    #[test]
    fn rendered_values_cannot_break_the_json() {
        let mut context = context();
        context
            .person
            .insert("name".into(), json!("\"}, \"admin\": true, {\"x\":\""));
        let doc = webhook(&[], r#"{"a":"Hi {{ person.name }}"}"#);
        let out = render_webhook(&doc, &context).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out.body).unwrap();
        assert_eq!(parsed["a"], json!("Hi \"}, \"admin\": true, {\"x\":\""));
        assert_eq!(parsed.as_object().unwrap().len(), 1);
    }

    #[test]
    fn webhook_header_names_must_be_tokens() {
        for name in ["X-A\r\nX-Evil: 1", "X:A", "X A", ""] {
            let doc = webhook(&[(name, "1")], "{}");
            assert_eq!(
                render_webhook(&doc, &context()),
                Err(RenderError::InvalidHeaderName),
                "{name:?}"
            );
        }
    }

    #[test]
    fn webhook_header_count_and_size_are_capped() {
        let names: Vec<String> = (0..=MAX_HEADER_COUNT).map(|n| format!("X-{n}")).collect();
        let headers: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "1")).collect();
        assert_eq!(
            render_webhook(&webhook(&headers, "{}"), &context()),
            Err(RenderError::TooManyHeaders)
        );
        // Each value is within its own limit, but together they pass the aggregate cap.
        let value = "v".repeat(MAX_HEADER_VALUE_BYTES);
        let names: Vec<String> = (0..5).map(|n| format!("X-{n}")).collect();
        let headers: Vec<(&str, &str)> =
            names.iter().map(|n| (n.as_str(), value.as_str())).collect();
        assert_eq!(
            render_webhook(&webhook(&headers, "{}"), &context()),
            Err(RenderError::HeadersTooLarge)
        );
    }

    #[test]
    fn headers_render_in_name_order_on_one_line() {
        let mut context = context();
        context.person.insert("name".into(), json!("a\nb"));
        let doc = webhook(
            &[("X-Zeta", "{{ person.name }}"), ("X-Alpha", "fixed")],
            "{}",
        );
        let out = render_webhook(&doc, &context).unwrap();
        assert_eq!(
            out.headers,
            vec![
                ("X-Alpha".to_owned(), "fixed".to_owned()),
                ("X-Zeta".to_owned(), "a b".to_owned())
            ]
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_refused() {
        let doc = webhook(&[], r#"{"a": {{ person.age }}}"#);
        assert_eq!(
            render_webhook(&doc, &context()),
            Err(RenderError::InvalidBody)
        );
    }

    #[test]
    fn templates_in_keys_are_refused() {
        let doc = webhook(&[], r#"{"{{ person.name }}":1}"#);
        assert_eq!(
            render_webhook(&doc, &context()),
            Err(RenderError::TemplateInKey)
        );
    }

    #[test]
    fn webhook_undefined_is_an_error() {
        let doc = webhook(&[], r#"{"a":"{{ person.nope }}"}"#);
        assert_eq!(
            render_webhook(&doc, &context()),
            Err(RenderError::Undefined { field: "body" })
        );
        let doc = webhook(&[], r#"{"a":"x {{ person.nope }}"}"#);
        assert_eq!(
            render_webhook(&doc, &context()),
            Err(RenderError::Undefined { field: "body" })
        );
    }

    #[test]
    fn the_body_size_limit_is_enforced_across_values() {
        let mut context = context();
        context.person.insert(
            "big".into(),
            json!("y".repeat(MAX_WEBHOOK_BODY_BYTES / 2 + 1)),
        );
        let doc = webhook(&[], r#"{"a":"{{ person.big }}","b":"{{ person.big }}"}"#);
        assert_eq!(
            render_webhook(&doc, &context),
            Err(RenderError::BodyTooLarge)
        );
        let doc = webhook(&[], r#"{"a":"x{{ person.big }}","b":"x{{ person.big }}"}"#);
        assert_eq!(
            render_webhook(&doc, &context),
            Err(RenderError::BodyTooLarge)
        );
    }

    #[test]
    fn single_tag_detection() {
        assert_eq!(single_tag("{{ a.b }}"), Some(" a.b "));
        assert_eq!(single_tag("{{a}}"), Some("a"));
        for text in [
            "x{{ a }}",
            "{{ a }}x",
            "{{ a }}{{ b }}",
            "{{- a }}",
            "{{ a -}}",
            "{{ }}",
            "{{ a }} {{ b }}",
            "plain",
        ] {
            assert_eq!(single_tag(text), None, "{text}");
        }
    }

    // ----- determinism ---------------------------------------------------------------------

    fn seeded_runner(seed: u64, cases: u32) -> TestRunner {
        let mut bytes = [0_u8; 32];
        for (index, chunk) in bytes.chunks_mut(8).enumerate() {
            let word = seed
                .wrapping_add(index as u64)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15);
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        let config = Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        };
        TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &bytes))
    }

    fn scalar() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            "[ -~]{0,12}".prop_map(serde_json::Value::from),
            any::<i32>().prop_map(serde_json::Value::from),
            any::<bool>().prop_map(serde_json::Value::from),
        ]
    }

    #[test]
    fn equal_inputs_render_to_equal_bytes() {
        let strategy = (
            proptest::collection::vec(("[a-z]{1,6}", scalar()), 0..8),
            "[a-z]{0,6}",
        );
        let doc = email(
            Category::Marketing,
            &format!(
                "<p>{{{{ person.name | default('x') }}}}</p>{{% for key, value in person | items %}}<i>{{{{ key }}}}={{{{ value }}}}</i>{{% endfor %}}{FOOTER}"
            ),
        );
        let hook = webhook(
            &[("X-N", "{{ person | length }}")],
            r#"{"p":"{{ person }}","n":"{{ person | length }}"}"#,
        );
        seeded_runner(0x5eed_0213, 64)
            .run(&strategy, |(entries, name)| {
                let mut forward = context();
                forward.person.clear();
                for (key, value) in &entries {
                    forward.person.insert(key.clone(), value.clone());
                }
                forward.person.insert("name".into(), json!(name));
                // The same entries, inserted in the opposite order.
                let mut backward = context();
                backward.person.clear();
                backward.person.insert("name".into(), json!(name));
                for (key, value) in entries.iter().rev() {
                    backward
                        .person
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
                let first = render_email(&doc, &forward, &live());
                let second = render_email(&doc, &forward, &live());
                let reordered = render_email(&doc, &backward, &live());
                prop_assert_eq!(&first, &second);
                prop_assert_eq!(&first, &reordered);
                let first = render_webhook(&hook, &forward);
                let second = render_webhook(&hook, &forward);
                prop_assert_eq!(&first, &second);
                Ok(())
            })
            .unwrap();
    }
}
