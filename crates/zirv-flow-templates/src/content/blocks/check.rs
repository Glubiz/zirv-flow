//! Publish validation of a block document (architecture 11.6). Every issue carries a block path
//! (see `model::block_at`): the id of the block, and the field when one applies.
//!
//! Two layers. [`check_sources`] needs no template engine: the shape, the limits, the URLs and
//! the sanitizer. [`validate`] adds what only a render shows: merge fields, syntax, the raw HTML
//! `mrml` must parse, and the unsubscribe link of the rendered sample.

use super::emit::UrlKind;
use super::model::{
    Block, BlocksContent, MAX_DOCUMENT_BYTES, MAX_LABEL_BYTES, MAX_RAW_HTML_BYTES,
    MAX_RICH_TEXT_BYTES, MAX_SUBJECT_SOURCE_BYTES, MAX_URL_BYTES, PREHEADER_DISPLAY_CHARS,
    SUBJECT_DISPLAY_CHARS, block_at, blocks_of, check_shape, doc_at,
};
use super::render::{BuildError, build};
use super::sanitize::{link_allowed, sanitize_source, violations};
use super::tags::starts_with_tag;
use crate::content::mjml::{MAX_DEPTH, MjmlError, compile, has_include};
use crate::context::SAMPLE_UNSUBSCRIBE_URL;
use crate::document::{Category, SCHEMA_VERSION, TemplateDocument};
use crate::error::RenderError;
use crate::issue::{FieldCatalog, Issue, code};
use crate::links::{link_urls, links_to};
use crate::sandbox::{MAX_HTML_BYTES, environment};
use crate::validate::{HTML, HTML_WARN_BYTES, SUBJECT, check_template, sample_context};

/// What a text field holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Subject,
    Preheader,
    Plain,
    Url(UrlKind),
    Rich,
    Raw,
}

impl Kind {
    const fn limit(self) -> usize {
        match self {
            Self::Subject | Self::Preheader => MAX_SUBJECT_SOURCE_BYTES,
            Self::Plain => MAX_LABEL_BYTES,
            Self::Url(_) => MAX_URL_BYTES,
            Self::Rich => MAX_RICH_TEXT_BYTES,
            Self::Raw => MAX_RAW_HTML_BYTES,
        }
    }
}

/// One text field of the document with its block path.
pub(crate) struct Field<'a> {
    pub(crate) at: String,
    pub(crate) source: &'a str,
    pub(crate) kind: Kind,
}

/// Every text field, in document order: the subject and preheader, then each block's fields.
pub(crate) fn fields(content: &BlocksContent) -> Vec<Field<'_>> {
    let mut out = vec![
        Field {
            at: doc_at("subject"),
            source: &content.subject,
            kind: Kind::Subject,
        },
        Field {
            at: doc_at("preheader"),
            source: &content.preheader,
            kind: Kind::Preheader,
        },
    ];
    for (_, block) in blocks_of(content) {
        let id = block.id();
        let mut field = |name: &str, source, kind| {
            out.push(Field {
                at: block_at(id, Some(name)),
                source,
                kind,
            });
        };
        match block {
            Block::Heading(heading) => field("text", &heading.text, Kind::Plain),
            Block::Text(rich) => field("content", &rich.content, Kind::Rich),
            Block::Image(image) => {
                field("src", &image.src, Kind::Url(UrlKind::ImageSource));
                field("alt", &image.alt, Kind::Plain);
                if let Some(href) = &image.href {
                    field("href", href, Kind::Url(UrlKind::Link));
                }
            }
            Block::Button(button) => {
                field("label", &button.label, Kind::Plain);
                field("url", &button.url, Kind::Url(UrlKind::Link));
            }
            Block::RawHtml(raw) => field("html", &raw.html, Kind::Raw),
            Block::Divider(_) | Block::Spacer(_) => {}
        }
    }
    out
}

fn has_insecure_scheme(url: &str) -> bool {
    url.trim_start()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
}

/// The checks that need no template engine: shape, limits, URL sources and the sanitizer. A
/// document over a limit gets that issue alone, so nothing past it is scanned.
pub(crate) fn check_sources(content: &BlocksContent) -> Vec<Issue> {
    let mut issues = check_shape(content);
    if issues.iter().any(|issue| issue.code == code::BLOCKS_LIMIT) {
        return issues;
    }
    let all = fields(content);
    let mut total = 0usize;
    let mut oversize = false;
    for field in &all {
        total = total.saturating_add(field.source.len());
        if field.source.len() > field.kind.limit() {
            issues.push(
                Issue::new(code::CONTENT_TOO_LARGE, field.at.clone())
                    .with_message(format!("at most {} bytes", field.kind.limit())),
            );
            oversize = true;
        }
    }
    if total > MAX_DOCUMENT_BYTES {
        issues.push(
            Issue::new(code::BLOCKS_LIMIT, doc_at("sections"))
                .with_message(format!("at most {MAX_DOCUMENT_BYTES} bytes of text in all")),
        );
        return issues;
    }
    if oversize {
        return issues;
    }
    for field in &all {
        match field.kind {
            Kind::Url(kind) => check_url(field, kind, &mut issues),
            Kind::Rich => check_rich(field, &mut issues),
            _ => {}
        }
    }
    issues
}

fn check_url(field: &Field<'_>, kind: UrlKind, issues: &mut Vec<Issue>) {
    let source = field.source;
    let allowed = match kind {
        UrlKind::Link => link_allowed(source),
        UrlKind::ImageSource => {
            starts_with_tag(source)
                || source
                    .trim_start()
                    .get(..8)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        }
    };
    if !allowed {
        let message = match kind {
            UrlKind::Link => "a link starts with https://, http://, mailto: or a merge tag",
            UrlKind::ImageSource => "an image source starts with https:// or a merge tag",
        };
        issues.push(Issue::new(code::INVALID_URL, field.at.clone()).with_message(message));
    } else if has_insecure_scheme(source) {
        issues.push(Issue::new(code::INSECURE_LINK, field.at.clone()));
    }
}

fn check_rich(field: &Field<'_>, issues: &mut Vec<Issue>) {
    let found = violations(field.source);
    if !found.is_empty() {
        let shown: Vec<&str> = found.iter().take(5).map(String::as_str).collect();
        issues.push(
            Issue::new(code::SANITIZER_REJECTED, field.at.clone())
                .with_message(format!("not allowed in rich text: {}", shown.join(", "))),
        );
    } else if link_urls(&sanitize_source(field.source).html)
        .iter()
        .any(|url| has_insecure_scheme(url))
    {
        issues.push(Issue::new(code::INSECURE_LINK, field.at.clone()));
    }
}

/// The issue for MJML that did not become HTML, at `at`.
fn mjml_issue(error: MjmlError, at: &str) -> Issue {
    match error {
        MjmlError::Include => Issue::new(code::MJML_INCLUDE, at),
        MjmlError::TooDeep => Issue::new(code::MJML_TOO_DEEP, at)
            .with_message(format!("elements nest deeper than {MAX_DEPTH} levels")),
        MjmlError::Parse {
            reason, excerpt, ..
        } => {
            Issue::new(code::MJML_PARSE_ERROR, at).with_message(format!("{reason} near: {excerpt}"))
        }
        MjmlError::Render => {
            Issue::new(code::MJML_PARSE_ERROR, at).with_message("the MJML could not be rendered")
        }
    }
}

/// Raw HTML must go through `mrml` as an `mj-raw` element: the sample render of the field is
/// put in one and compiled, so the issue names the block that holds the problem.
fn check_raw(field: &Field<'_>, rendered: Option<&str>, issues: &mut Vec<Issue>) {
    if has_include(field.source) {
        issues.push(Issue::new(code::MJML_INCLUDE, field.at.clone()));
        return;
    }
    let Some(rendered) = rendered else {
        return;
    };
    let probe = format!(
        "<mjml><mj-body><mj-section><mj-column><mj-raw>{rendered}</mj-raw></mj-column></mj-section></mj-body></mjml>"
    );
    if let Err(error) = compile(&probe) {
        issues.push(mjml_issue(error, &field.at));
    }
}

/// The publish checks of a block document.
pub(crate) fn validate(doc: &TemplateDocument, catalog: &FieldCatalog) -> Vec<Issue> {
    if doc.schema != SCHEMA_VERSION {
        return vec![Issue::new(code::UNSUPPORTED_SCHEMA, "/schema")];
    }
    let Some(content) = &doc.blocks else {
        return vec![Issue::new(code::MISSING_CONTENT, "/blocks")];
    };
    let mut issues = check_sources(content);
    if issues
        .iter()
        .any(|issue| matches!(issue.code, code::BLOCKS_LIMIT | code::CONTENT_TOO_LARGE))
    {
        return issues;
    }

    if content
        .sender
        .as_deref()
        .is_none_or(|sender| sender.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_SENDER, doc_at("sender")));
    }
    if content
        .lang
        .as_deref()
        .is_none_or(|lang| lang.trim().is_empty())
    {
        issues.push(Issue::new(code::MISSING_LANG, doc_at("lang")));
    }

    let env = environment();
    let sample = sample_context(catalog);
    for field in fields(content) {
        let kind = if field.kind == Kind::Subject {
            SUBJECT
        } else {
            HTML
        };
        let checked = check_template(
            &env,
            catalog,
            &sample,
            &field.at,
            field.source,
            kind,
            &mut issues,
        );
        match field.kind {
            Kind::Subject => display_length(
                &checked.sample,
                &field.at,
                SUBJECT_DISPLAY_CHARS,
                code::SUBJECT_TOO_LONG,
                &mut issues,
            ),
            Kind::Preheader => display_length(
                &checked.sample,
                &field.at,
                PREHEADER_DISPLAY_CHARS,
                code::PREHEADER_TOO_LONG,
                &mut issues,
            ),
            Kind::Raw => check_raw(&field, checked.sample.as_deref(), &mut issues),
            _ => {}
        }
    }
    for (_, block) in blocks_of(content) {
        if let Block::Image(image) = block
            && image.alt.trim().is_empty()
        {
            issues.push(Issue::new(
                code::IMAGE_WITHOUT_ALT,
                block_at(&image.id, Some("alt")),
            ));
        }
    }

    if issues.iter().any(Issue::is_blocking) {
        return issues;
    }
    check_sample_render(content, catalog, &env, &mut issues);
    issues
}

fn display_length(
    sample: &Option<String>,
    at: &str,
    limit: usize,
    issue: &'static str,
    issues: &mut Vec<Issue>,
) {
    if sample
        .as_deref()
        .is_some_and(|text| text.trim().chars().count() > limit)
    {
        issues.push(Issue::new(issue, at).with_message(format!("longer than {limit} characters")));
    }
}

/// Render the whole document against the sample context, as a send would, and check what comes
/// out: the unsubscribe link, the size, and whether `mrml` accepted the MJML.
fn check_sample_render(
    content: &BlocksContent,
    catalog: &FieldCatalog,
    env: &minijinja::Environment<'static>,
    issues: &mut Vec<Issue>,
) {
    const AT: &str = "/blocks";
    let values = sample_context(catalog);
    match build(content, env, &values, false) {
        Ok(built) => {
            if content.category == Category::Marketing
                && !links_to(&built.html, SAMPLE_UNSUBSCRIBE_URL)
            {
                issues.push(Issue::new(code::MISSING_UNSUBSCRIBE, AT));
            }
            if built.html.len() >= HTML_WARN_BYTES {
                issues.push(Issue::new(code::HTML_NEAR_CLIP, AT));
            }
        }
        Err(BuildError::Mjml(error)) => issues.push(mjml_issue(error, AT)),
        Err(BuildError::Render(error)) => {
            if matches!(error, RenderError::OutputTooLarge { field: "html" }) {
                issues.push(
                    Issue::new(code::CONTENT_TOO_LARGE, AT)
                        .with_message(format!("the rendered HTML is over {MAX_HTML_BYTES} bytes")),
                );
                return;
            }
            // The sample cannot stand for every person (for example `| int` on a string
            // sample), so this warns. The source must still put the unsubscribe URL in a link.
            issues.push(Issue::new(code::SAMPLE_RENDER_FAILED, AT).with_message(error.code()));
            let mentions = fields(content)
                .iter()
                .any(|field| field.source.contains("system.unsubscribe_url"));
            if content.category == Category::Marketing && !mentions {
                issues.push(Issue::new(code::MISSING_UNSUBSCRIBE, AT));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use zirv_flow_core::people::TraitType;

    use super::super::model::{Image, MAX_BLOCKS, MAX_SECTIONS, RawHtml, Section};
    use super::*;
    use crate::content::{content_type_of, validate_document};
    use crate::issue::{Severity, severity_of};

    fn newsletter() -> TemplateDocument {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/blocks/newsletter.doc.json");
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn catalog() -> FieldCatalog {
        FieldCatalog {
            person: BTreeMap::from([
                ("name".to_owned(), TraitType::String),
                ("plan".to_owned(), TraitType::String),
            ]),
            ..FieldCatalog::default()
        }
    }

    /// The issues of the newsletter after `change`, as code and path.
    fn after(change: impl FnOnce(&mut BlocksContent)) -> Vec<(&'static str, String)> {
        let mut doc = newsletter();
        change(doc.blocks.as_mut().expect("block content"));
        validate(&doc, &catalog())
            .into_iter()
            .map(|issue| (issue.code, issue.at))
            .collect()
    }

    fn block<'a>(content: &'a mut BlocksContent, id: &str) -> &'a mut Block {
        content
            .sections
            .iter_mut()
            .flat_map(|section| section.columns.iter_mut().flatten())
            .find(|block| block.id() == id)
            .expect("a block with that id")
    }

    fn entry(code: &'static str, at: &str) -> (&'static str, String) {
        (code, at.to_owned())
    }

    #[test]
    fn the_reference_newsletter_has_no_issue() {
        assert_eq!(after(|_| {}), Vec::new());
    }

    #[test]
    fn an_unknown_merge_field_is_blocking_and_names_the_block_and_the_field() {
        let mut doc = newsletter();
        if let Block::Heading(heading) = block(doc.blocks.as_mut().unwrap(), "intro_title") {
            heading.text = "Hi {{ person.nope }}".into();
        }
        let issues = validate(&doc, &catalog());
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].code, code::UNKNOWN_MERGE_FIELD);
        assert_eq!(issues[0].at, "/blocks/block/intro_title/text");
        assert_eq!(issues[0].field.as_deref(), Some("person.nope"));
        assert!(issues[0].is_blocking());
        // The subject and the preheader have document-level paths.
        assert_eq!(
            after(|c| c.subject = "{{ event.nope }}".into()),
            [entry(code::UNKNOWN_MERGE_FIELD, "/blocks/subject")]
        );
        assert_eq!(
            after(|c| c.preheader = "{{ person.nope }}".into()),
            [entry(code::UNKNOWN_MERGE_FIELD, "/blocks/preheader")]
        );
    }

    #[test]
    fn a_minijinja_syntax_error_is_blocking_with_the_path_of_its_field() {
        assert_eq!(
            after(|c| {
                if let Block::Button(button) = block(c, "cta_button") {
                    button.label = "{% if %}".into();
                }
            }),
            [entry(code::SYNTAX_ERROR, "/blocks/block/cta_button/label")]
        );
        assert_eq!(
            after(|c| {
                if let Block::Text(rich) = block(c, "intro_text") {
                    rich.content = "<p>{{ person.name | nosuchfilter }}</p>".into();
                }
            }),
            [entry(
                code::SYNTAX_ERROR,
                "/blocks/block/intro_text/content"
            )]
        );
    }

    #[test]
    fn a_marketing_email_without_an_unsubscribe_link_is_blocking() {
        let removed = after(|c| {
            c.sections.retain(|section| section.id != "footer");
        });
        assert_eq!(removed, [entry(code::MISSING_UNSUBSCRIBE, "/blocks")]);
        // A transactional email needs none.
        assert_eq!(
            after(|c| {
                c.sections.retain(|section| section.id != "footer");
                c.category = Category::Transactional;
            }),
            Vec::new()
        );
        // A bare URL that is not a link does not count.
        assert_eq!(
            after(|c| {
                if let Block::Text(rich) = block(c, "footer_text") {
                    rich.content = "<p>{{ system.unsubscribe_url }}</p>".into();
                }
            }),
            [entry(code::MISSING_UNSUBSCRIBE, "/blocks")]
        );
    }

    #[test]
    fn a_missing_sender_is_blocking_and_a_missing_lang_only_warns() {
        let issues = after(|c| {
            c.sender = Some(" ".into());
            c.lang = None;
        });
        assert_eq!(
            issues,
            [
                entry(code::MISSING_SENDER, "/blocks/sender"),
                entry(code::MISSING_LANG, "/blocks/lang"),
            ]
        );
        assert_eq!(severity_of(code::MISSING_SENDER), Severity::Blocking);
        assert_eq!(severity_of(code::MISSING_LANG), Severity::Warning);
    }

    #[test]
    fn a_schema_or_content_failure_is_blocking() {
        let mut doc = newsletter();
        doc.schema = 2;
        assert_eq!(validate(&doc, &catalog())[0].code, code::UNSUPPORTED_SCHEMA);
        let mut empty = newsletter();
        empty.blocks = None;
        assert_eq!(validate(&empty, &catalog())[0].code, code::MISSING_CONTENT);
    }

    #[test]
    fn rich_text_that_the_sanitizer_would_change_is_blocking() {
        for (content, parts) in [
            ("<p>a</p><script>alert(1)</script>", "<script>"),
            ("<p onclick=\"x()\">a</p>", "onclick="),
            ("<a href=\"javascript:alert(1)\">a</a>", "href="),
            ("<a href=\"data:text/html,x\">a</a>", "href="),
            ("<img src=\"https://a.example/x.png\">", "<img>"),
        ] {
            let mut doc = newsletter();
            if let Block::Text(rich) = block(doc.blocks.as_mut().unwrap(), "intro_text") {
                rich.content = content.into();
            }
            let issues = validate(&doc, &catalog());
            assert_eq!(issues.len(), 1, "{content}: {issues:?}");
            assert_eq!(issues[0].code, code::SANITIZER_REJECTED);
            assert_eq!(issues[0].at, "/blocks/block/intro_text/content");
            assert!(
                issues[0].message.as_deref().unwrap_or("").contains(parts),
                "{content}: {issues:?}"
            );
        }
    }

    #[test]
    fn raw_html_that_mrml_cannot_parse_is_blocking_on_its_block() {
        let raw = |html: &str| {
            let html = html.to_owned();
            after(move |c| {
                c.sections[1].columns[0].push(Block::RawHtml(RawHtml {
                    id: "raw1".into(),
                    html,
                }));
            })
        };
        assert_eq!(raw("<p>fine</p>"), Vec::new());
        assert_eq!(
            raw("<div><b>unclosed</div>"),
            [entry(code::MJML_PARSE_ERROR, "/blocks/block/raw1/html")]
        );
        assert_eq!(
            raw("<mj-include path=\"x\" />"),
            [entry(code::MJML_INCLUDE, "/blocks/block/raw1/html")]
        );
        assert_eq!(
            raw("</mj-raw></mj-column></mj-section></mj-body></mjml><p>x"),
            [entry(code::MJML_PARSE_ERROR, "/blocks/block/raw1/html")]
        );
        let deep = "<a>".repeat(200);
        assert_eq!(
            raw(&deep),
            [entry(code::MJML_TOO_DEEP, "/blocks/block/raw1/html")]
        );
    }

    #[test]
    fn a_document_over_a_limit_is_blocking_and_is_not_scanned_further() {
        let many_sections = after(|c| {
            for index in 0..=MAX_SECTIONS {
                c.sections.push(Section {
                    id: format!("extra{index}"),
                    background_color: None,
                    padding: None,
                    columns: vec![vec![]],
                });
            }
        });
        assert_eq!(
            many_sections,
            [entry(code::BLOCKS_LIMIT, "/blocks/sections")]
        );

        let many_blocks = after(|c| {
            c.sections[0].columns[0] = (0..=MAX_BLOCKS)
                .map(|n| {
                    Block::Spacer(crate::content::blocks::Spacer {
                        id: format!("sp{n}"),
                        height: 1,
                    })
                })
                .collect();
        });
        assert_eq!(many_blocks, [entry(code::BLOCKS_LIMIT, "/blocks/sections")]);

        let long_field = after(|c| {
            if let Block::Text(rich) = block(c, "intro_text") {
                rich.content = format!("<p>{}</p>", "x".repeat(MAX_RICH_TEXT_BYTES));
            }
        });
        assert_eq!(
            long_field,
            [entry(
                code::CONTENT_TOO_LARGE,
                "/blocks/block/intro_text/content"
            )]
        );

        let too_much_text = after(|c| {
            for n in 0..6 {
                c.sections[0].columns[0].push(Block::RawHtml(RawHtml {
                    id: format!("big{n}"),
                    html: "x".repeat(MAX_RAW_HTML_BYTES),
                }));
            }
        });
        assert_eq!(
            too_much_text,
            [entry(code::BLOCKS_LIMIT, "/blocks/sections")]
        );
    }

    #[test]
    fn shape_issues_come_through_validation_with_their_paths() {
        assert_eq!(
            after(|c| {
                c.settings.content_width = 900;
                c.sections[1].columns.extend([vec![], vec![], vec![]]);
                c.sections[2].id = "hero".into();
            }),
            [
                entry(code::INVALID_SETTING, "/blocks/settings/content_width"),
                entry(code::INVALID_COLUMNS, "/blocks/block/intro/columns"),
                entry(code::DUPLICATE_BLOCK_ID, "/blocks/block/hero"),
            ]
        );
    }

    #[test]
    fn a_url_that_is_not_https_http_mailto_or_a_merge_tag_is_blocking() {
        let url = |value: &str| {
            let value = value.to_owned();
            after(move |c| {
                if let Block::Button(button) = block(c, "cta_button") {
                    button.url = value;
                }
            })
        };
        for good in [
            "https://a.example",
            "mailto:a@b.example",
            "{{ system.preferences_url }}",
        ] {
            assert_eq!(url(good), Vec::new(), "{good}");
        }
        for bad in ["javascript:alert(1)", "data:text/html,x", "/relative", ""] {
            assert_eq!(
                url(bad),
                [entry(code::INVALID_URL, "/blocks/block/cta_button/url")],
                "{bad}"
            );
        }
        // An image source must be https.
        let src = |value: &str| {
            let value = value.to_owned();
            after(move |c| {
                if let Block::Image(image) = block(c, "hero_image") {
                    image.src = value;
                }
            })
        };
        assert_eq!(src("{{ person.name }}"), Vec::new());
        for bad in [
            "http://a.example/i.png",
            "mailto:a@b.example",
            "data:image/png;base64,AA",
        ] {
            assert_eq!(
                src(bad),
                [entry(code::INVALID_URL, "/blocks/block/hero_image/src")],
                "{bad}"
            );
        }
    }

    #[test]
    fn warnings_do_not_block_and_each_has_its_path() {
        let image = after(|c| {
            c.sections[0].columns[0].push(Block::Image(Image {
                id: "noalt".into(),
                src: "https://a.example/i.png".into(),
                alt: " ".into(),
                href: None,
                width: None,
            }));
        });
        assert_eq!(
            image,
            [entry(code::IMAGE_WITHOUT_ALT, "/blocks/block/noalt/alt")]
        );

        assert_eq!(
            after(|c| c.subject = "s".repeat(SUBJECT_DISPLAY_CHARS + 1)),
            [entry(code::SUBJECT_TOO_LONG, "/blocks/subject")]
        );
        assert_eq!(
            after(|c| c.subject = "s".repeat(SUBJECT_DISPLAY_CHARS)),
            Vec::new()
        );
        assert_eq!(
            after(|c| c.preheader = "p".repeat(PREHEADER_DISPLAY_CHARS + 1)),
            [entry(code::PREHEADER_TOO_LONG, "/blocks/preheader")]
        );

        let insecure = after(|c| {
            if let Block::Button(button) = block(c, "cta_button") {
                button.url = "http://shop.example/spring".into();
            }
            if let Block::Text(rich) = block(c, "intro_text") {
                rich.content = "<p><a href=\"HTTP://a.example\">x</a></p>".into();
            }
        });
        assert_eq!(
            insecure,
            [
                entry(code::INSECURE_LINK, "/blocks/block/intro_text/content"),
                entry(code::INSECURE_LINK, "/blocks/block/cta_button/url"),
            ]
        );

        let near_clip = after(|c| {
            c.sections[1].columns[0].push(Block::RawHtml(RawHtml {
                id: "fat".into(),
                html: format!("<p>{}</p>", "x".repeat(95_000)),
            }));
        });
        assert_eq!(near_clip, [entry(code::HTML_NEAR_CLIP, "/blocks")]);

        for warning in [
            code::IMAGE_WITHOUT_ALT,
            code::SUBJECT_TOO_LONG,
            code::PREHEADER_TOO_LONG,
            code::INSECURE_LINK,
            code::HTML_NEAR_CLIP,
        ] {
            assert_eq!(severity_of(warning), Severity::Warning, "{warning}");
        }
    }

    #[test]
    fn a_sample_that_cannot_stand_for_a_person_warns_and_the_source_must_still_link() {
        // `| int` on the string sample fails the sample render, which is not the marketer's fault.
        let failing = |keep_link: bool| {
            after(move |c| {
                if let Block::Heading(heading) = block(c, "intro_title") {
                    heading.text = "{{ person.name | int + 1 }}".into();
                }
                if !keep_link {
                    c.sections.retain(|section| section.id != "footer");
                }
            })
        };
        assert_eq!(
            failing(true),
            [entry(code::SAMPLE_RENDER_FAILED, "/blocks")]
        );
        assert_eq!(
            failing(false),
            [
                entry(code::SAMPLE_RENDER_FAILED, "/blocks"),
                entry(code::MISSING_UNSUBSCRIBE, "/blocks"),
            ]
        );
    }

    #[test]
    fn the_registry_validates_a_block_document_through_the_blocks_type() {
        let doc = newsletter();
        assert_eq!(content_type_of(&doc).unwrap().id(), "blocks");
        assert_eq!(
            validate_document(&doc, &catalog()).unwrap(),
            crate::validate(&doc, &catalog())
        );
        let mut broken = newsletter();
        broken.blocks.as_mut().unwrap().sender = None;
        assert_eq!(
            validate_document(&broken, &catalog()).unwrap()[0].code,
            code::MISSING_SENDER
        );
    }

    #[test]
    fn the_new_codes_are_in_the_blocking_and_warning_lists() {
        for blocking in [
            code::BLOCKS_LIMIT,
            code::DUPLICATE_BLOCK_ID,
            code::INVALID_BLOCK_ID,
            code::INVALID_COLUMNS,
            code::INVALID_SETTING,
            code::INVALID_URL,
            code::SANITIZER_REJECTED,
        ] {
            assert!(code::BLOCKING.contains(&blocking), "{blocking}");
        }
        for warning in [
            code::IMAGE_WITHOUT_ALT,
            code::SUBJECT_TOO_LONG,
            code::PREHEADER_TOO_LONG,
        ] {
            assert!(code::WARNING.contains(&warning), "{warning}");
        }
        assert_eq!(Kind::Plain.limit(), MAX_LABEL_BYTES);
    }
}
