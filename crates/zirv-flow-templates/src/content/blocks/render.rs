//! Rendering a block email (architecture 11.5): MiniJinja per text field, MJML from the tree,
//! `mrml`, and the text part from the tree. The post-render steps are the shared ones of
//! `render_email`.

use minijinja::{Environment, Value};

use super::check::check_sources;
use super::emit::{Fields, Plain, UrlKind, emit};
use super::sanitize::{Cleaned, enforce, html_to_text, sanitize_source, scheme_allowed};
use super::tags::escape_literals;
use crate::content::mjml::{MjmlError, compile};
use crate::context::RenderContext;
use crate::document::{Category, Channel, Mode, SCHEMA_VERSION, TemplateDocument};
use crate::error::RenderError;
use crate::links::unescape;
use crate::render::{map_mjml_error, one_line};
use crate::sandbox::{
    MAX_HTML_BYTES, MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, environment, render_string,
};

use super::model::BlocksContent;

/// Why the pipeline stopped: MiniJinja or the checks (a [`RenderError`]), or `mrml`.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BuildError {
    Render(RenderError),
    Mjml(MjmlError),
}

impl From<RenderError> for BuildError {
    fn from(error: RenderError) -> Self {
        Self::Render(error)
    }
}

impl From<MjmlError> for BuildError {
    fn from(error: MjmlError) -> Self {
        Self::Mjml(error)
    }
}

/// The HTML and text part of a rendered block email.
pub(crate) struct Built {
    pub(crate) html: String,
    pub(crate) text: String,
}

/// Fields rendered by MiniJinja in the sandbox.
struct Rendered<'a> {
    env: &'a Environment<'static>,
    values: &'a Value,
    /// Whether a rendered URL must be one a link may use. Validation renders a sample whose
    /// values are not URLs, so it does not check them.
    check_urls: bool,
}

impl Fields for Rendered<'_> {
    type Error = RenderError;

    fn plain(&mut self, source: &str) -> Result<Plain, RenderError> {
        // The literal text is escaped first and MiniJinja escapes what it merges, so the result
        // is text in both parts. Unescaping it gives the plain text, with the merged values as
        // they were.
        let html = render_string(
            self.env,
            "field.html",
            "html",
            &escape_literals(source),
            self.values,
            MAX_HTML_BYTES,
        )?;
        let text = unescape(&html);
        Ok(Plain { html, text })
    }

    fn url(&mut self, source: &str, kind: UrlKind) -> Result<Plain, RenderError> {
        let value = self.plain(source)?;
        let url = value.text.trim();
        let allowed = match kind {
            UrlKind::Link => scheme_allowed(url),
            UrlKind::ImageSource => url
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://")),
        };
        if self.check_urls && !allowed {
            return Err(RenderError::InvalidBlocks);
        }
        Ok(value)
    }

    fn rich(&mut self, source: &str) -> Result<Cleaned, RenderError> {
        let cleaned = sanitize_source(source);
        let rendered = render_string(
            self.env,
            "rich.html",
            "html",
            &cleaned.html,
            self.values,
            MAX_HTML_BYTES,
        )?;
        let html = enforce(&rendered);
        let text = html_to_text(&html);
        Ok(Cleaned { html, text })
    }

    fn raw(&mut self, source: &str) -> Result<String, RenderError> {
        render_string(
            self.env,
            "raw.html",
            "html",
            source,
            self.values,
            MAX_HTML_BYTES,
        )
    }
}

/// Render the tree and turn it into HTML: fields, MJML, `mrml`, then the caps on the result.
pub(crate) fn build(
    content: &BlocksContent,
    env: &Environment<'static>,
    values: &Value,
    check_urls: bool,
) -> Result<Built, BuildError> {
    let mut fields = Rendered {
        env,
        values,
        check_urls,
    };
    let emitted = emit(content, &mut fields)?;
    let compiled = compile(&emitted.mjml)?;
    if compiled.html.len() > MAX_HTML_BYTES {
        return Err(RenderError::OutputTooLarge { field: "html" }.into());
    }
    if emitted.text.len() > MAX_TEXT_BYTES {
        return Err(RenderError::OutputTooLarge { field: "text" }.into());
    }
    Ok(Built {
        html: compiled.html,
        text: emitted.text,
    })
}

/// The parts of a rendered block email before the post-render steps.
pub(crate) struct Parts {
    pub(crate) subject: String,
    pub(crate) html: String,
    pub(crate) text: String,
    pub(crate) category: Category,
}

/// Render a block document against `context`.
pub(crate) fn render(
    doc: &TemplateDocument,
    context: &RenderContext,
) -> Result<Parts, RenderError> {
    if doc.schema != SCHEMA_VERSION {
        return Err(RenderError::UnsupportedSchema);
    }
    let content = match (&doc.blocks, doc.channel, doc.mode) {
        (Some(content), Channel::Email, Mode::Blocks) => content,
        _ => return Err(RenderError::WrongDocument),
    };
    if check_sources(content)
        .iter()
        .any(|issue| issue.is_blocking())
    {
        return Err(RenderError::InvalidBlocks);
    }
    let env = environment();
    let values = Value::from_serialize(context);
    let subject = one_line(&render_string(
        &env,
        "subject.txt",
        "subject",
        &content.subject,
        &values,
        MAX_SUBJECT_BYTES,
    )?);
    let built = build(content, &env, &values, true).map_err(|error| match error {
        BuildError::Render(error) => error,
        BuildError::Mjml(error) => map_mjml_error(error),
    })?;
    Ok(Parts {
        subject,
        html: built.html,
        text: built.text,
        category: content.category,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::super::{Block, blocks_to_mjml};
    use super::*;
    use crate::document::{EmailContent, Syntax};
    use crate::links::{Hooks, links_to};
    use crate::render::{Rendered as Email, render_email};

    const CORPUS: [&str; 9] = [
        "heading",
        "text",
        "image",
        "button",
        "divider",
        "spacer",
        "raw_html",
        "sections",
        "newsletter",
    ];

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocks")
    }

    fn read(name: &str) -> String {
        fs::read_to_string(fixtures().join(name))
            .unwrap()
            .replace("\r\n", "\n")
    }

    fn doc(name: &str) -> TemplateDocument {
        serde_json::from_str(&read(&format!("{name}.doc.json"))).unwrap()
    }

    fn context() -> RenderContext {
        serde_json::from_str(&read("context.json")).unwrap()
    }

    fn render_doc(doc: &TemplateDocument) -> Result<Email, RenderError> {
        render_email(doc, &context(), &Hooks::untracked())
    }

    fn check(file: &str, actual: &str) {
        let path = fixtures().join(file);
        if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
            fs::write(&path, actual).unwrap();
        }
        let expected = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing {}; bless with ZIRV_FLOW_BLESS=1", path.display()));
        assert_eq!(expected.replace("\r\n", "\n"), actual, "{file}");
    }

    /// The first block of the first column of the first section, for a test to change.
    fn first_block(doc: &mut TemplateDocument) -> &mut Block {
        &mut doc.blocks.as_mut().expect("block content").sections[0].columns[0][0]
    }

    #[test]
    fn the_golden_corpus_renders_to_its_expected_bytes_twice() {
        for name in CORPUS {
            let first = render_doc(&doc(name)).unwrap();
            check(&format!("{name}.html"), &first.html);
            check(&format!("{name}.txt"), &first.text);
            assert_eq!(
                first,
                render_doc(&doc(name)).unwrap(),
                "{name} is not stable"
            );
        }
    }

    #[test]
    fn the_newsletter_is_table_layout_with_inline_styles_mso_comments_and_a_mobile_query() {
        let html = render_doc(&doc("newsletter")).unwrap().html;
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<table"), "table-based layout");
        assert!(html.contains(" style=\""), "inline styles");
        assert!(html.contains("<!--[if mso"), "conditional comments");
        assert!(html.contains("<![endif]-->"));
        assert!(
            html.contains("@media only screen and (min-width:480px)"),
            "mobile media query"
        );
        assert!(html.contains("max-width:600px"), "content width");
        assert!(html.contains("lang=\"en\""));
        assert!(html.contains("mj-column-per-100"), "single column");
        assert!(html.contains("mj-column-per-50"), "two columns");
        assert!(html.contains("https://img.example/hero.png"));
        assert!(html.contains("href=\"https://shop.example/spring\""));
        assert!(links_to(&html, "https://u.example/u/1?s=a&t=b"));
        let sections = render_doc(&doc("sections")).unwrap().html;
        assert!(sections.contains("mj-column-per-33"), "three columns");
    }

    #[test]
    fn the_settings_change_the_content_width_and_the_colours() {
        let mut wide = doc("heading");
        if let Some(content) = wide.blocks.as_mut() {
            content.settings.content_width = 720;
            content.settings.background_color = "#123456".into();
            content.settings.font_family = "georgia".into();
        }
        let html = render_doc(&wide).unwrap().html;
        assert!(html.contains("max-width:720px"), "{html}");
        assert!(html.contains("#123456"));
        assert!(html.contains("Georgia"));
    }

    #[test]
    fn the_converted_mjml_renders_to_the_bytes_of_the_block_render() {
        for name in CORPUS {
            let block_doc = doc(name);
            let content = block_doc.blocks.as_ref().expect("block content");
            let converted = blocks_to_mjml(content).unwrap();
            let code = TemplateDocument::email_code(EmailContent {
                subject: content.subject.clone(),
                sender: content.sender.clone(),
                lang: content.lang.clone(),
                category: content.category,
                syntax: Syntax::Mjml,
                html: converted.mjml.clone(),
                text: converted.text.clone(),
            });
            let from_blocks = render_doc(&block_doc).unwrap();
            let from_code = render_doc(&code).unwrap();
            assert_eq!(from_blocks.html, from_code.html, "{name}");
            assert_eq!(from_blocks.subject, from_code.subject, "{name}");
            assert_eq!(from_blocks.headers, from_code.headers, "{name}");
        }
    }

    #[test]
    fn the_converted_mjml_keeps_the_merge_tags_and_the_text_part_keeps_them_too() {
        let block_doc = doc("newsletter");
        let content = block_doc.blocks.as_ref().expect("block content");
        let converted = blocks_to_mjml(content).unwrap();
        assert!(converted.mjml.contains("Hello {{ person.name }}"));
        assert!(
            converted
                .mjml
                .contains("href=\"{{ system.unsubscribe_url }}\"")
        );
        assert!(
            converted
                .text
                .contains("Unsubscribe: {{ system.unsubscribe_url }}"),
            "{}",
            converted.text
        );
    }

    fn set_id(block: &mut Block, id: &str) {
        let slot = match block {
            Block::Heading(b) => &mut b.id,
            Block::Text(b) => &mut b.id,
            Block::Image(b) => &mut b.id,
            Block::Button(b) => &mut b.id,
            Block::Divider(b) => &mut b.id,
            Block::Spacer(b) => &mut b.id,
            Block::RawHtml(b) => &mut b.id,
        };
        *slot = id.to_owned();
    }

    #[test]
    fn the_text_part_comes_from_the_tree_for_every_block_type() {
        let mut all = doc("newsletter");
        let others = [
            "heading", "text", "image", "button", "divider", "spacer", "raw_html",
        ];
        let content = all.blocks.as_mut().expect("block content");
        for (index, name) in others.into_iter().enumerate() {
            for mut section in doc(name).blocks.expect("block content").sections {
                section.id = format!("x{index}");
                for (n, block) in section.columns.iter_mut().flatten().enumerate() {
                    set_id(block, &format!("x{index}_{n}"));
                }
                content.sections.push(section);
            }
        }
        let rendered = render_doc(&all).unwrap();
        check("all_blocks.html", &rendered.html);
        check("all_blocks.txt", &rendered.text);
        // The text has the headings, the paragraphs, the list items, the links as
        // `label: url`, and none of the markup.
        for part in [
            "Hello Ann <b>O'Neil</b>",
            "Our spring collection is here.",
            "See the collection: https://shop.example/spring",
            "- One",
            "1. First",
            "Shop now, Ann <b>O'Neil</b>: https://shop.example/offer?a=1&b=2",
            "Unsubscribe: https://u.example/u/1?s=a&t=b",
            "----------",
        ] {
            assert!(rendered.text.contains(part), "{part}\n{}", rendered.text);
        }
        assert!(!rendered.text.contains("<p>"));
        assert!(!rendered.text.contains("&amp;"));
    }

    #[test]
    fn a_merge_value_with_markup_in_rich_text_adds_no_element() {
        let mut context = context();
        context.person.insert(
            "name".into(),
            json!(
                "</p><script>alert(1)</script><img src=x onerror=y><a href=\"javascript:z\">q</a>"
            ),
        );
        let rendered = render_email(&doc("text"), &context, &Hooks::untracked()).unwrap();
        for forbidden in ["<script", "<img", "href=\"javascript"] {
            assert!(!rendered.html.contains(forbidden), "{forbidden}");
        }
        assert!(rendered.html.contains("&lt;script&gt;"));
        // The same document with a plain name has the same number of elements.
        let plain = render_doc(&doc("text")).unwrap().html;
        for element in ["<table", "<ul", "<li", "<a ", "<b>"] {
            assert_eq!(
                plain.matches(element).count(),
                rendered.html.matches(element).count(),
                "{element}"
            );
        }
        // The text part holds the value as text.
        assert!(rendered.text.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn a_merge_value_that_names_a_bad_scheme_in_a_rich_text_link_is_cleaned_after_the_render() {
        let mut block_doc = doc("text");
        if let Block::Text(rich) = first_block(&mut block_doc) {
            rich.content = "<p><a href=\"{{ person.link }}\">go</a></p>".into();
        }
        let mut context = context();
        context
            .person
            .insert("link".into(), json!("javascript:alert(1)"));
        let rendered = render_email(&block_doc, &context, &Hooks::untracked()).unwrap();
        assert!(!rendered.html.contains("javascript:"), "{}", rendered.html);
        context
            .person
            .insert("link".into(), json!("https://ok.example/a?b=1&c=2"));
        let ok = render_email(&block_doc, &context, &Hooks::untracked()).unwrap();
        assert!(links_to(&ok.html, "https://ok.example/a?b=1&c=2"));
    }

    #[test]
    fn a_rendered_url_that_a_link_may_not_use_is_refused() {
        let mut block_doc = doc("button");
        if let Block::Button(button) = first_block(&mut block_doc) {
            button.url = "{{ person.link }}".into();
        }
        for link in ["javascript:alert(1)", "data:text/html,x", "/relative", ""] {
            let mut context = context();
            context.person.insert("link".into(), json!(link));
            assert_eq!(
                render_email(&block_doc, &context, &Hooks::untracked()),
                Err(RenderError::InvalidBlocks),
                "{link}"
            );
        }
    }

    #[test]
    fn a_document_the_checks_refuse_is_not_rendered() {
        let mut block_doc = doc("heading");
        if let Some(content) = block_doc.blocks.as_mut() {
            content.settings.content_width = 5;
        }
        assert_eq!(render_doc(&block_doc), Err(RenderError::InvalidBlocks));
        let mut other = doc("heading");
        other.schema = 2;
        assert_eq!(render_doc(&other), Err(RenderError::UnsupportedSchema));
        let mut wrong = doc("heading");
        wrong.channel = Channel::Webhook;
        assert_eq!(render_doc(&wrong), Err(RenderError::WrongDocument));
    }

    #[test]
    fn a_marketing_document_without_the_unsubscribe_link_is_refused_at_render() {
        let mut block_doc = doc("heading");
        if let Some(content) = block_doc.blocks.as_mut() {
            content.category = Category::Marketing;
        }
        assert_eq!(render_doc(&block_doc), Err(RenderError::MissingUnsubscribe));
    }

    #[test]
    fn raw_html_is_passed_through_and_an_include_is_still_refused() {
        let html = render_doc(&doc("raw_html")).unwrap().html;
        assert!(
            html.contains("Raw for Ann &lt;b&gt;O&#x27;Neil&lt;&#x2f;b&gt; &amp; <b>co</b>"),
            "{html}"
        );
        let mut block_doc = doc("raw_html");
        if let Block::RawHtml(raw) = first_block(&mut block_doc) {
            raw.html = "<mj-include path=\"/etc/passwd\" />".into();
        }
        assert_eq!(render_doc(&block_doc), Err(RenderError::MjmlInclude));
    }

    #[test]
    fn the_subject_is_one_line_and_a_missing_value_is_an_undefined_error() {
        let mut block_doc = doc("heading");
        if let Some(content) = block_doc.blocks.as_mut() {
            content.subject = "Hi {{ person.name }}\nthere".into();
        }
        assert_eq!(
            render_doc(&block_doc).unwrap().subject,
            "Hi Ann <b>O'Neil</b> there"
        );
        if let Some(content) = block_doc.blocks.as_mut() {
            content.subject = "Hi {{ person.nobody }}".into();
        }
        assert_eq!(
            render_doc(&block_doc),
            Err(RenderError::Undefined { field: "subject" })
        );
    }

    #[test]
    fn marketing_mail_gets_the_unsubscribe_headers() {
        let rendered = render_doc(&doc("newsletter")).unwrap();
        assert_eq!(rendered.headers.len(), 2);
        assert_eq!(rendered.headers[0].0, "List-Unsubscribe");
    }
}
