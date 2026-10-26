//! Rendering a block email (architecture 11.5): MiniJinja per text field, MJML from the tree,
//! `mrml`, and the text part from the tree. The post-render steps are the shared ones of
//! `render_email`.

use minijinja::{Environment, Value};

use super::check::{check_sources, fields};
use super::emit::{Fields, Plain, UrlKind, emit};
use super::sanitize::{
    Cleaned, enforce, has_mjml_tag, html_to_text, sanitize_source, scheme_allowed,
};
use super::tags::escape_literals;
use crate::content::mjml::{MjmlError, compile};
use crate::context::{RenderContext, RenderExtras};
use crate::document::{Category, Channel, Mode, SCHEMA_VERSION, TemplateDocument};
use crate::error::RenderError;
use crate::links::{starts_with_ignore_case, unescape};
use crate::render::{has_template_syntax, map_mjml_error, one_line};
use crate::sandbox::{
    FUEL, MAX_HTML_BYTES, MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, environment, render_string,
};

use super::model::{BlocksContent, ImageSrc, ThemeSlot};

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
    /// The ids of the sections and blocks a false `visible_if` dropped.
    pub(crate) hidden: Vec<String>,
}

/// The MiniJinja fuel of a whole block document: five times the fuel of one code template,
/// because a document is many small templates. Each field that holds template syntax gets an
/// equal share of it (at most [`FUEL`], the single-template budget), so the fields together can
/// never cost more than this, however many there are. The share is fixed by the count, not by
/// what the earlier fields used: MiniJinja reports no fuel use for a finished render, so a floor
/// taken from a shared remainder cannot be kept. With more than five such fields a field gets
/// less than a code template's `FUEL` (666 with the most the limits allow), which a loop-free
/// field never comes near.
pub(crate) const DOCUMENT_FUEL: u64 = 5 * FUEL;

/// The sandbox environment for one block document, with its share of the document fuel.
pub(crate) fn document_environment(content: &BlocksContent) -> Environment<'static> {
    let tagged = fields(content)
        .iter()
        .filter(|field| has_template_syntax(field.source))
        .count()
        .max(1);
    let mut env = environment();
    let share = DOCUMENT_FUEL / u64::try_from(tagged).unwrap_or(u64::MAX);
    env.set_fuel(Some(share.min(FUEL)));
    env
}

/// Drop the characters XML cannot hold (U+0000 to U+0008, U+000B, U+000C, U+000E to U+001F):
/// `mrml` and the tokenizer behind the text part refuse them.
pub(crate) fn strip_invalid(text: String) -> String {
    let invalid = |c: char| matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}');
    if text.chars().any(invalid) {
        text.chars().filter(|c| !invalid(*c)).collect()
    } else {
        text
    }
}

/// Fields rendered by MiniJinja in the sandbox.
struct Rendered<'a> {
    env: &'a Environment<'static>,
    values: &'a Value,
    extras: &'a RenderExtras,
    /// Whether a rendered URL must be one a link may use. Validation renders a sample whose
    /// values are not URLs, so it does not check them. A validation render also shows a block
    /// whose condition has no result, because it has no person to ask about.
    check_urls: bool,
    /// Bytes the rendered fields have used of the `MAX_HTML_BYTES` they share.
    spent: usize,
}

impl Rendered<'_> {
    /// Render one field into what is left of the shared output budget.
    fn render(&mut self, name: &'static str, source: &str) -> Result<String, RenderError> {
        let left = MAX_HTML_BYTES.saturating_sub(self.spent);
        let out = render_string(self.env, name, "html", source, self.values, left)?;
        self.spent += out.len();
        Ok(strip_invalid(out))
    }
}

impl Fields for Rendered<'_> {
    type Error = RenderError;

    fn plain(&mut self, source: &str) -> Result<Plain, RenderError> {
        // The literal text is escaped first and MiniJinja escapes what it merges, so the result
        // is text in both parts. Unescaping it gives the plain text, with the merged values as
        // they were.
        let html = self.render("field.html", &escape_literals(source))?;
        // Autoescape leaves no `<` or `"` in a rendered value; one here came through `|safe` or
        // `{% autoescape false %}` and could add markup to the MJML or break out of an attribute.
        if html.contains(['<', '"']) {
            return Err(RenderError::InvalidBlocks);
        }
        let text = unescape(&html);
        Ok(Plain { html, text })
    }

    fn url(&mut self, source: &str, kind: UrlKind) -> Result<Plain, RenderError> {
        let value = self.plain(source)?;
        let url = value.text.trim();
        let allowed = match kind {
            UrlKind::Link => scheme_allowed(url),
            UrlKind::ImageSource => starts_with_ignore_case(url, "https://"),
        };
        if self.check_urls && !allowed {
            return Err(RenderError::InvalidBlocks);
        }
        Ok(value)
    }

    fn rich(&mut self, source: &str) -> Result<Cleaned, RenderError> {
        let cleaned = sanitize_source(source);
        let rendered = self.render("rich.html", &cleaned.html)?;
        let html = enforce(&rendered);
        let text = html_to_text(&html);
        Ok(Cleaned { html, text })
    }

    fn raw(&mut self, source: &str) -> Result<String, RenderError> {
        let html = self.render("raw.html", source)?;
        if has_mjml_tag(&html) {
            return Err(RenderError::InvalidBlocks);
        }
        Ok(html)
    }

    fn image_source(&mut self, src: &ImageSrc) -> Result<Plain, RenderError> {
        let url = match src {
            ImageSrc::Url(url) => url,
            ImageSrc::Asset(id) => self
                .extras
                .asset_url(id)
                .ok_or(RenderError::BrokenAssetReference)?,
            ImageSrc::Theme(ThemeSlot::Logo) => self
                .extras
                .theme_logo_url()
                .ok_or(RenderError::BrokenThemeReference)?,
        };
        self.url(url, UrlKind::ImageSource)
    }

    fn visible(&mut self, id: &str) -> Result<bool, RenderError> {
        match self.extras.conditions.get(id) {
            Some(shown) => Ok(*shown),
            None if self.check_urls => Err(RenderError::ConditionUnresolved),
            None => Ok(true),
        }
    }
}

/// Render the tree and turn it into HTML: fields, MJML, `mrml`, then the caps on the result.
pub(crate) fn build(
    content: &BlocksContent,
    env: &Environment<'static>,
    values: &Value,
    extras: &RenderExtras,
    check_urls: bool,
) -> Result<Built, BuildError> {
    let mut fields = Rendered {
        env,
        values,
        extras,
        check_urls,
        spent: 0,
    };
    let emitted = emit(content, &mut fields)?;
    // The rendered fields hold at most `MAX_HTML_BYTES` together; the MJML around them and the
    // text part are held to the same cap before `mrml` runs.
    if emitted.mjml.len() + emitted.text.len() > MAX_HTML_BYTES {
        return Err(RenderError::OutputTooLarge { field: "html" }.into());
    }
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
        hidden: emitted.hidden,
    })
}

/// The parts of a rendered block email before the post-render steps.
pub(crate) struct Parts {
    pub(crate) subject: String,
    pub(crate) html: String,
    pub(crate) text: String,
    pub(crate) category: Category,
    pub(crate) hidden: Vec<String>,
}

/// Render a block document against `context`, with the caller's resolved `extras`.
pub(crate) fn render(
    doc: &TemplateDocument,
    context: &RenderContext,
    extras: &RenderExtras,
) -> Result<Parts, RenderError> {
    if doc.schema != SCHEMA_VERSION {
        return Err(RenderError::UnsupportedSchema);
    }
    let content = match (&doc.blocks, doc.channel, doc.mode) {
        (Some(content), Channel::Email, Mode::Blocks) if !doc.is_ambiguous() => content,
        _ => return Err(RenderError::WrongDocument),
    };
    if check_sources(content)
        .iter()
        .any(|issue| issue.is_blocking())
    {
        return Err(RenderError::InvalidBlocks);
    }
    let env = document_environment(content);
    let values = Value::from_serialize(context);
    let subject = one_line(&render_string(
        &env,
        "subject.txt",
        "subject",
        &content.subject,
        &values,
        MAX_SUBJECT_BYTES,
    )?);
    let built = build(content, &env, &values, extras, true).map_err(|error| match error {
        BuildError::Render(error) => error,
        BuildError::Mjml(error) => map_mjml_error(error),
    })?;
    Ok(Parts {
        subject,
        html: built.html,
        text: built.text,
        category: content.category,
        hidden: built.hidden,
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
    use crate::migrate::read_doc;
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
        read_doc(&read(&format!("{name}.doc.json")))
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
            Block::Snippet(b) => &mut b.id,
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
        other.schema = 3;
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

    fn doc_of(blocks: Vec<Block>) -> TemplateDocument {
        let mut content = BlocksContent::new("Hi");
        content.sender = Some("0192b1a4-7c3e-7000-8000-00000000a001".into());
        content.lang = Some("en".into());
        content.category = Category::Transactional;
        content.sections.push(super::super::Section {
            id: "s".into(),
            background_color: None,
            padding: None,
            columns: vec![blocks],
            visible_if: None,
        });
        TemplateDocument::email_blocks(content)
    }

    fn heading(id: &str, text: &str) -> Block {
        Block::Heading(super::super::Heading {
            id: id.into(),
            level: 2,
            text: text.into(),
            align: None,
            visible_if: None,
        })
    }

    fn raw(id: &str, html: &str) -> Block {
        Block::RawHtml(super::super::RawHtml {
            id: id.into(),
            html: html.into(),
            visible_if: None,
        })
    }

    fn rich(id: &str, content: &str) -> Block {
        Block::Text(super::super::RichText {
            id: id.into(),
            content: content.into(),
            align: None,
            visible_if: None,
        })
    }

    #[test]
    fn the_rendered_fields_share_one_output_budget_and_mrml_is_not_reached() {
        use crate::content::mjml::parser_call_count;

        let blocks = (0..4)
            .map(|n| raw(&format!("r{n}"), "{{ person.big }}"))
            .collect();
        let mut context = context();
        // Each field alone is far under the cap; four of them are not.
        context
            .person
            .insert("big".into(), json!("x".repeat(250_000)));
        let one = doc_of(vec![raw("r0", "{{ person.big }}")]);
        assert!(render_email(&one, &context, &Hooks::untracked()).is_ok());

        let before = parser_call_count();
        let many = doc_of(blocks);
        assert_eq!(
            render_email(&many, &context, &Hooks::untracked()),
            Err(RenderError::OutputTooLarge { field: "html" })
        );
        assert_eq!(parser_call_count(), before, "mrml must not see it");
    }

    #[test]
    fn the_fuel_is_bounded_for_the_whole_document_not_for_each_field() {
        let spin = "{% for i in range(30000) %}{% endfor %}x";
        // One such field is within the single-template budget.
        let one = doc_of(vec![heading("h0", spin)]);
        assert!(render_doc(&one).is_ok());
        // Forty of them are not, though each is.
        let many = doc_of((0..40).map(|n| heading(&format!("h{n}"), spin)).collect());
        assert_eq!(
            render_doc(&many),
            Err(RenderError::OutOfFuel { field: "html" })
        );
    }

    #[test]
    fn a_control_character_in_a_merge_value_is_dropped_from_html_and_text() {
        let mut context = context();
        context
            .person
            .insert("ctl".into(), json!("A\u{c}B\u{0}C\u{1f}D\tE"));
        let block_doc = doc_of(vec![
            heading("h", "{{ person.ctl }}"),
            rich("t", "<p>{{ person.ctl }}</p>"),
            raw("r", "<p>{{ person.ctl }}</p>"),
        ]);
        let rendered = render_email(&block_doc, &context, &Hooks::untracked()).unwrap();
        assert!(rendered.html.contains("ABCD\tE"), "{}", rendered.html);
        assert!(
            !rendered
                .html
                .chars()
                .any(|c| c.is_control() && !"\n\t\r".contains(c)),
            "{}",
            rendered.html
        );
        assert_eq!(
            rendered.text.matches("ABCD").count(),
            2,
            "{:?}",
            rendered.text
        );
        let short = doc_of(vec![heading("h", "{{ person.ctl }}")]);
        context.person.insert("ctl".into(), json!("A\u{c}B"));
        let rendered = render_email(&short, &context, &Hooks::untracked()).unwrap();
        assert!(rendered.html.contains(">AB<"), "{}", rendered.html);
        assert_eq!(rendered.text, "AB\n");
    }

    #[test]
    fn a_plain_field_cannot_carry_markup_even_when_autoescape_is_bypassed() {
        let mut context = context();
        context.person.insert(
            "bio".into(),
            json!("</mj-text><mj-raw><img src=x onerror=1>"),
        );
        for text in [
            "{{ person.bio | safe }}",
            "{% autoescape false %}{{ person.bio }}{% endautoescape %}",
            "{{ person.bio | safe | upper }}",
        ] {
            let block_doc = doc_of(vec![heading("h", text)]);
            assert_eq!(
                render_email(&block_doc, &context, &Hooks::untracked()),
                Err(RenderError::InvalidBlocks),
                "{text}"
            );
        }
        // Every plain field kind is covered, and an attribute-breaking quote too.
        context
            .person
            .insert("quote".into(), json!("x\" onload=\"y"));
        let button = Block::Button(super::super::Button {
            id: "b".into(),
            label: "Go".into(),
            url: "https://a.example/{{ person.quote | safe }}".into(),
            align: Default::default(),
            visible_if: None,
        });
        assert_eq!(
            render_email(&doc_of(vec![button]), &context, &Hooks::untracked()),
            Err(RenderError::InvalidBlocks)
        );
        // An escaped value is fine.
        let escaped = doc_of(vec![heading("h", "{{ person.bio }}")]);
        assert!(render_email(&escaped, &context, &Hooks::untracked()).is_ok());
    }

    #[test]
    fn a_closer_inside_a_string_of_a_merge_tag_renders_as_written() {
        let label = r#"a {{ person.plan | default("}}") }} b {{ "}}" }}"#;
        let block_doc = doc_of(vec![Block::Button(super::super::Button {
            id: "b".into(),
            label: label.into(),
            url: "https://a.example".into(),
            align: Default::default(),
            visible_if: None,
        })]);
        let rendered = render_doc(&block_doc).unwrap();
        assert_eq!(rendered.text, "a pro b }}: https://a.example\n");
        assert!(rendered.html.contains("a pro b }}"), "{}", rendered.html);
    }

    #[test]
    fn a_non_breaking_space_in_rich_text_is_a_space_in_the_text_part() {
        let block_doc = doc_of(vec![rich("t", "<p>Hello&nbsp;world</p>")]);
        assert_eq!(render_doc(&block_doc).unwrap().text, "Hello world\n");
        let converted = blocks_to_mjml(block_doc.blocks.as_ref().unwrap()).unwrap();
        assert_eq!(converted.text, "Hello world\n");
    }

    fn doc_value(name: &str) -> serde_json::Value {
        serde_json::from_str(&read(&format!("{name}.doc.json"))).unwrap()
    }

    fn render_with(
        doc: &TemplateDocument,
        extras: &crate::context::RenderExtras,
    ) -> Result<Email, RenderError> {
        crate::render::render_email_with(doc, &context(), &Hooks::untracked(), extras)
    }

    fn extras_of(conditions: &[(&str, bool)]) -> crate::context::RenderExtras {
        crate::context::RenderExtras {
            conditions: conditions
                .iter()
                .map(|(id, shown)| ((*id).to_owned(), *shown))
                .collect(),
            ..Default::default()
        }
    }

    const ASSET: &str = "0192b1a4-7c3e-7000-8000-00000000b001";

    #[test]
    fn every_schema_one_fixture_migrates_to_its_golden_document_and_renders_the_same_bytes() {
        for name in CORPUS {
            let migrated = crate::migrate_to_current(doc_value(name)).unwrap();
            let mut text = serde_json::to_string_pretty(&migrated).unwrap();
            text.push('\n');
            check(&format!("migrations/{name}.v2.json"), &text);

            let golden = read_doc(&read(&format!("migrations/{name}.v2.json")));
            assert_eq!(golden.schema, 2, "{name}");
            let rendered = render_doc(&golden).unwrap();
            assert_eq!(rendered.html, read(&format!("{name}.html")), "{name}");
            assert_eq!(rendered.text, read(&format!("{name}.txt")), "{name}");
            assert!(rendered.hidden.is_empty());
        }
    }

    #[test]
    fn a_false_condition_drops_the_block_and_lists_it() {
        let mut value = doc_value("newsletter");
        value["blocks"]["sections"][1]["columns"][0][1]["visible_if"] = json!({"all": []});
        let conditioned = read_doc(&value.to_string());
        let baseline = render_doc(&read_doc(&read("newsletter.doc.json"))).unwrap();

        let shown = render_with(&conditioned, &extras_of(&[("intro_text", true)])).unwrap();
        assert_eq!(shown, baseline);

        let hidden = render_with(&conditioned, &extras_of(&[("intro_text", false)])).unwrap();
        assert_eq!(
            hidden.hidden,
            [crate::render::HiddenBlock {
                id: "intro_text".into()
            }]
        );
        assert!(baseline.html.contains("collection is here"));
        assert!(!hidden.html.contains("collection is here"));
        assert!(!hidden.text.contains("collection is here"));
        assert!(hidden.html.contains("Hello"), "the sibling stays");
    }

    #[test]
    fn a_false_section_condition_drops_the_section_and_lists_only_the_section() {
        let mut value = doc_value("newsletter");
        value["blocks"]["sections"][2]["visible_if"] = json!({"x": 1});
        let conditioned = read_doc(&value.to_string());
        let rendered = render_with(&conditioned, &extras_of(&[("features", false)])).unwrap();
        assert_eq!(
            rendered.hidden,
            [crate::render::HiddenBlock {
                id: "features".into()
            }]
        );
        assert!(!rendered.html.contains("Prices that stay put"));
    }

    #[test]
    fn a_condition_without_a_result_is_a_typed_render_error() {
        let mut value = doc_value("newsletter");
        value["blocks"]["sections"][1]["columns"][0][1]["visible_if"] = json!({"all": []});
        let conditioned = read_doc(&value.to_string());
        assert_eq!(
            render_with(&conditioned, &Default::default()),
            Err(RenderError::ConditionUnresolved)
        );
        assert_eq!(
            RenderError::ConditionUnresolved.code(),
            "condition_unresolved"
        );
    }

    #[test]
    fn an_unknown_field_on_a_conditioned_block_still_fails() {
        let mut value = doc_value("newsletter");
        value["blocks"]["sections"][1]["columns"][0][1]["hidden_if"] = json!(true);
        assert_eq!(
            super::super::parse_template_document(&value).unwrap_err(),
            super::super::BlocksError::UnknownField
        );
    }

    fn image_doc(src: serde_json::Value) -> TemplateDocument {
        let mut value = doc_value("image");
        value["blocks"]["sections"][0]["columns"][0][0]["src"] = src;
        read_doc(&value.to_string())
    }

    #[test]
    fn image_sources_resolve_from_the_asset_the_url_and_the_theme_logo() {
        let url = image_doc(json!({"url": "https://img.example/hero.png"}));
        assert_eq!(
            render_doc(&url).unwrap(),
            render_doc(&doc("image")).unwrap()
        );

        let asset = image_doc(json!({"asset": ASSET}));
        let mut extras = crate::context::RenderExtras::default();
        extras
            .assets
            .insert(ASSET.into(), "https://cdn.example/a.png".into());
        let rendered = render_with(&asset, &extras).unwrap();
        assert!(rendered.html.contains("https://cdn.example/a.png"));
        assert_eq!(
            render_with(&asset, &Default::default()),
            Err(RenderError::BrokenAssetReference)
        );

        let logo = image_doc(json!({"theme": "logo"}));
        assert_eq!(
            render_with(&logo, &extras),
            Err(RenderError::BrokenThemeReference)
        );
        extras.theme = Some(crate::theme::ThemeTokens {
            logo: Some(ASSET.into()),
            ..Default::default()
        });
        let rendered = render_with(&logo, &extras).unwrap();
        assert!(rendered.html.contains("https://cdn.example/a.png"));
    }

    #[test]
    fn rendering_twice_gives_equal_bytes() {
        use proptest::prelude::*;
        let mut runner = proptest::test_runner::TestRunner::default();
        runner
            .run(
                &("[a-zA-Z0-9 ,.!]{0,40}", any::<bool>()),
                |(text, shown)| {
                    let mut value = doc_value("heading");
                    value["blocks"]["sections"][0]["columns"][0][0]["text"] = json!(text);
                    value["blocks"]["sections"][0]["columns"][0][0]["visible_if"] = json!({});
                    let doc = read_doc(&value.to_string());
                    let id = value["blocks"]["sections"][0]["columns"][0][0]["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let extras = extras_of(&[(id.as_str(), shown)]);
                    prop_assert_eq!(render_with(&doc, &extras), render_with(&doc, &extras));
                    Ok(())
                },
            )
            .unwrap();
    }
}
