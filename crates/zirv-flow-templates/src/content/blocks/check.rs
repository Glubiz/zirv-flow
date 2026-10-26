//! Publish validation of a block document (architecture 11.6). Every issue carries a block path
//! (see `model::block_at`): the id of the block, and the field when one applies.
//!
//! Two layers. [`check_sources`] needs no template engine: the shape, the limits, the URLs and
//! the sanitizer. [`validate`] adds what only a render shows: merge fields, syntax, the raw HTML
//! `mrml` must parse, and the unsubscribe link of the rendered sample.

use super::emit::UrlKind;
use super::look::{Broken, Look, THEME_FOOTER_ID};
use super::model::{
    Block, BlocksContent, ImageSrc, MAX_DOCUMENT_BYTES, MAX_LABEL_BYTES, MAX_RAW_HTML_BYTES,
    MAX_RICH_TEXT_BYTES, MAX_SUBJECT_SOURCE_BYTES, MAX_URL_BYTES, PREHEADER_DISPLAY_CHARS,
    SUBJECT_DISPLAY_CHARS, ThemeSlot, block_at, blocks_of, check_block_numbers, check_shape,
    conditions, doc_at,
};
use super::render::{BuildError, build, document_environment};
use super::sanitize::{has_mjml_tag, link_allowed, sanitize_source, violations};
use super::snippet::{MAX_SNIPPET_BLOCKS, SnippetContent};
use super::tags::{Protected, escape_literals, starts_with_tag};
use crate::content::mjml::{MAX_DEPTH, MjmlError, compile, has_include};
use crate::context::{RenderContext, RenderExtras, SAMPLE_UNSUBSCRIBE_URL};
use crate::document::{Category, SCHEMA_VERSION, TemplateDocument};
use crate::error::RenderError;
use crate::issue::{FieldCatalog, Issue, code};
use crate::links::{link_urls, links_to, starts_with_ignore_case, unescape};
use crate::sandbox::MAX_HTML_BYTES;
use crate::validate::{HTML, HTML_WARN_BYTES, SUBJECT, Scope, check_template_in};

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

/// The blocks of the effective content in document order, each with the path of the snippet
/// block or `theme-footer` it came through (`None` for the document's own). A snippet block is
/// replaced by the blocks of its content; one without content has none here, and a snippet block
/// inside a snippet is kept for the checks to refuse.
pub(crate) fn effective_blocks<'a>(
    content: &'a BlocksContent,
    extras: &'a RenderExtras,
    footer: Option<&'a SnippetContent>,
) -> Vec<(Option<String>, &'a Block)> {
    let mut out = Vec::new();
    for (_, block) in blocks_of(content) {
        match block {
            Block::Snippet(reference) => {
                if let Some(snippet) = extras.snippets.get(&reference.snippet_id) {
                    let base = block_at(block.id(), None);
                    out.extend(
                        snippet
                            .blocks
                            .iter()
                            .map(|inner| (Some(base.clone()), inner)),
                    );
                }
            }
            _ => out.push((None, block)),
        }
    }
    if let Some(footer) = footer {
        let base = block_at(THEME_FOOTER_ID, None);
        out.extend(
            footer
                .blocks
                .iter()
                .map(|inner| (Some(base.clone()), inner)),
        );
    }
    out
}

/// A path inside a snippet block or the theme footer: the path of that block, then the path of
/// the inner block.
fn nested_at(base: Option<&str>, at: String) -> String {
    match base {
        None => at,
        Some(base) => format!("{base}{}", at.strip_prefix("/blocks").unwrap_or(&at)),
    }
}

/// Every text field of the effective content, in document order: the subject and preheader,
/// then each block's fields, with those of a snippet and of the theme footer in place.
pub(crate) fn fields<'a>(
    content: &'a BlocksContent,
    extras: &'a RenderExtras,
    footer: Option<&'a SnippetContent>,
) -> Vec<Field<'a>> {
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
    for (base, block) in effective_blocks(content, extras, footer) {
        let start = out.len();
        block_fields(block, &mut out);
        for field in &mut out[start..] {
            field.at = nested_at(base.as_deref(), std::mem::take(&mut field.at));
        }
    }
    out
}

/// The text fields of one block, in the order they are checked.
pub(crate) fn block_fields<'a>(block: &'a Block, out: &mut Vec<Field<'a>>) {
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
            if let ImageSrc::Url(url) = &image.src {
                field("src", url, Kind::Url(UrlKind::ImageSource));
            }
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
        Block::Divider(_) | Block::Spacer(_) | Block::Snippet(_) => {}
    }
}

fn has_insecure_scheme(url: &str) -> bool {
    starts_with_ignore_case(url.trim_start(), "http://")
}

/// The checks that need no template engine: shape, limits, URL sources and the sanitizer. A
/// document over a limit gets that issue alone, so nothing past it is scanned.
pub(crate) fn check_sources(content: &BlocksContent, extras: &RenderExtras) -> Vec<Issue> {
    let mut issues = check_shape(content);
    if issues.iter().any(|issue| issue.code == code::BLOCKS_LIMIT) {
        return issues;
    }
    let own = RenderExtras::default();
    let all = fields(content, &own, None);
    check_texts(&all, &mut issues);
    // The snippets and the footer were checked when they were published; they are checked again
    // here because what is rendered is what the caller passed. A reference with no content is
    // `check_references`' business.
    let (look, _) = Look::resolve(content, extras);
    for (_, block) in blocks_of(content) {
        if let Block::Snippet(reference) = block
            && let Some(snippet) = extras.snippets.get(&reference.snippet_id)
        {
            check_nested(&block_at(block.id(), None), snippet, &mut issues);
        }
    }
    if let Some(footer) = look.footer {
        check_nested(&block_at(THEME_FOOTER_ID, None), footer, &mut issues);
    }
    issues
}

/// The checks of a snippet's content at the place it is used: no section, a bounded block
/// count, no snippet block, no condition, valid numbers, and the text checks of its fields.
fn check_nested(base: &str, snippet: &SnippetContent, issues: &mut Vec<Issue>) {
    if !snippet.sections.is_empty() {
        issues.push(Issue::new(code::SNIPPET_SECTION, base));
    }
    if snippet.blocks.len() > MAX_SNIPPET_BLOCKS {
        issues.push(
            Issue::new(code::BLOCKS_LIMIT, base)
                .with_message(format!("at most {MAX_SNIPPET_BLOCKS} blocks")),
        );
        return;
    }
    let mut all: Vec<Field<'_>> = Vec::new();
    for block in &snippet.blocks {
        if matches!(block, Block::Snippet(_)) {
            issues.push(Issue::new(
                code::SNIPPET_NESTED,
                nested_at(Some(base), block_at(block.id(), None)),
            ));
        }
        if block.visible_if().is_some() {
            issues.push(Issue::new(
                code::SNIPPET_CONDITION,
                nested_at(Some(base), block_at(block.id(), Some("visible_if"))),
            ));
        }
        let start = issues.len();
        check_block_numbers(block, issues);
        for issue in &mut issues[start..] {
            issue.at = nested_at(Some(base), std::mem::take(&mut issue.at));
        }
        let first = all.len();
        block_fields(block, &mut all);
        for field in &mut all[first..] {
            field.at = nested_at(Some(base), std::mem::take(&mut field.at));
        }
    }
    check_texts(&all, issues);
}

/// The size limits of the text fields and the checks of their URLs, rich text and raw HTML. A
/// field over its limit, or a document over the total, gets that issue alone.
pub(crate) fn check_texts(all: &[Field<'_>], issues: &mut Vec<Issue>) {
    let mut total = 0usize;
    let mut oversize = false;
    for field in all {
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
        return;
    }
    if oversize {
        return;
    }
    for field in all {
        match field.kind {
            Kind::Url(kind) => check_url(field, kind, issues),
            Kind::Rich => check_rich(field, issues),
            Kind::Raw if raw_has_mjml_tag(field.source) => issues.push(
                Issue::new(code::MJML_IN_RAW_HTML, field.at.clone())
                    .with_message("raw HTML holds no mj-* element: it sits inside an mj-raw"),
            ),
            _ => {}
        }
    }
}

/// Whether the raw HTML source holds an `mj-*` tag. The MiniJinja tags are set aside first, so
/// the scan reads the markup around them.
fn raw_has_mjml_tag(source: &str) -> bool {
    has_mjml_tag(&Protected::new(source).text)
}

/// Whether a URL source starts with a merge tag that is not one of the system's own signed URLs:
/// its scheme is known only when a message is rendered.
fn is_merge_led(url: &str) -> bool {
    url.trim_start().strip_prefix("{{").is_some_and(|rest| {
        !rest
            .trim_start_matches(['-', '+'])
            .trim_start()
            .starts_with("system.")
    })
}

fn check_url(field: &Field<'_>, kind: UrlKind, issues: &mut Vec<Issue>) {
    let source = field.source;
    let allowed = match kind {
        UrlKind::Link => link_allowed(source),
        UrlKind::ImageSource => {
            starts_with_tag(source) || starts_with_ignore_case(source.trim_start(), "https://")
        }
    };
    if !allowed {
        let message = match kind {
            UrlKind::Link => "a link starts with https://, http://, mailto: or a merge tag",
            UrlKind::ImageSource => "an image source starts with https:// or a merge tag",
        };
        issues.push(Issue::new(code::INVALID_URL, field.at.clone()).with_message(message));
    } else if is_merge_led(source) {
        issues.push(Issue::new(code::URL_FROM_MERGE_FIELD, field.at.clone()));
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
    } else {
        let links = link_urls(&sanitize_source(field.source).html);
        if links.iter().any(|url| is_merge_led(url)) {
            issues.push(Issue::new(code::URL_FROM_MERGE_FIELD, field.at.clone()));
        }
        if links.iter().any(|url| has_insecure_scheme(url)) {
            issues.push(Issue::new(code::INSECURE_LINK, field.at.clone()));
        }
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
    // A tag in the source is reported by `check_sources`; one that only a value brings is
    // found in the sample.
    if raw_has_mjml_tag(field.source) {
        return;
    }
    let Some(rendered) = rendered else {
        return;
    };
    if has_mjml_tag(rendered) {
        issues.push(Issue::new(code::MJML_IN_RAW_HTML, field.at.clone()));
        return;
    }
    let probe = format!(
        "<mjml><mj-body><mj-section><mj-column><mj-raw>{rendered}</mj-raw></mj-column></mj-section></mj-body></mjml>"
    );
    if let Err(error) = compile(&probe) {
        issues.push(mjml_issue(error, &field.at));
    }
}

/// The publish checks of a block document.
pub(crate) fn validate(
    doc: &TemplateDocument,
    catalog: &FieldCatalog,
    extras: &RenderExtras,
) -> Vec<Issue> {
    if doc.schema != SCHEMA_VERSION {
        return vec![Issue::new(code::UNSUPPORTED_SCHEMA, "/schema")];
    }
    let Some(content) = &doc.blocks else {
        return vec![Issue::new(code::MISSING_CONTENT, "/blocks")];
    };
    let mut issues = check_sources(content, extras);
    if issues
        .iter()
        .any(|issue| matches!(issue.code, code::BLOCKS_LIMIT | code::CONTENT_TOO_LARGE))
    {
        return issues;
    }
    // #307 removes this when the evaluator supplies condition results.
    for (id, _) in conditions(content) {
        issues.push(
            Issue::new(
                code::CONDITION_UNSUPPORTED,
                block_at(id, Some("visible_if")),
            )
            .with_message("Visibility conditions can't be published yet."),
        );
    }
    let (look, broken) = Look::resolve(content, extras);
    check_references(content, extras, &look, broken, &mut issues);

    // The sender is the id of a sender identity, as in a code document.
    if content
        .sender
        .as_deref()
        .is_none_or(|sender| !crate::validate::is_uuid(sender.trim()))
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

    let env = document_environment(content, extras, look.footer);
    let sample = look.values(&RenderContext::sample(catalog), extras);
    for field in fields(content, extras, look.footer) {
        check_field_in(
            &env,
            catalog,
            look.theme.is_some(),
            &sample,
            &field,
            &mut issues,
        );
    }
    for (base, block) in effective_blocks(content, extras, look.footer) {
        if let Block::Image(image) = block
            && image.alt.trim().is_empty()
        {
            issues.push(Issue::new(
                code::IMAGE_WITHOUT_ALT,
                nested_at(base.as_deref(), block_at(&image.id, Some("alt"))),
            ));
        }
    }

    if issues.iter().any(Issue::is_blocking) {
        return issues;
    }
    check_sample_render(content, &look, catalog, &env, extras, &mut issues);
    issues
}

/// The merge field, syntax and sample checks of one text field, where no theme is in effect:
/// every `theme.*` name is unknown.
pub(crate) fn check_field(
    env: &minijinja::Environment<'static>,
    catalog: &FieldCatalog,
    sample: &minijinja::Value,
    field: &Field<'_>,
    issues: &mut Vec<Issue>,
) {
    check_field_in(env, catalog, false, sample, field, issues);
}

/// [`check_field`], where `theme` says whether a theme is in effect and its values are known.
fn check_field_in(
    env: &minijinja::Environment<'static>,
    catalog: &FieldCatalog,
    theme: bool,
    sample: &minijinja::Value,
    field: &Field<'_>,
    issues: &mut Vec<Issue>,
) {
    let kind = if field.kind == Kind::Subject {
        SUBJECT
    } else {
        HTML
    };
    // Check the text MiniJinja is given: the plain fields have their literal text escaped
    // and the rich text is the sanitized one, so a syntax error that only they have is
    // found here, with the block path.
    let source = match field.kind {
        Kind::Preheader | Kind::Plain | Kind::Url(_) => escape_literals(field.source),
        Kind::Rich => sanitize_source(field.source).html,
        Kind::Subject | Kind::Raw => field.source.to_owned(),
    };
    let scope = Scope { catalog, theme };
    let checked = check_template_in(env, scope, sample, &field.at, &source, kind, issues);
    if matches!(field.kind, Kind::Preheader | Kind::Plain | Kind::Url(_))
        && checked
            .sample
            .as_deref()
            .is_some_and(|text| text.contains(['<', '"']))
    {
        issues.push(
            Issue::new(code::MARKUP_IN_PLAIN_FIELD, field.at.clone())
                .with_message("a value of this field is not escaped: |safe or autoescape off"),
        );
    }
    let shown = checked.sample.as_deref().map(unescape);
    match field.kind {
        Kind::Subject => display_length(
            &shown,
            &field.at,
            SUBJECT_DISPLAY_CHARS,
            code::SUBJECT_TOO_LONG,
            issues,
        ),
        Kind::Preheader => display_length(
            &shown,
            &field.at,
            PREHEADER_DISPLAY_CHARS,
            code::PREHEADER_TOO_LONG,
            issues,
        ),
        Kind::Raw => check_raw(field, checked.sample.as_deref(), issues),
        _ => {}
    }
}

/// The issues of what MJML code cannot hold: a condition on a section or block, and an image
/// that is not a plain URL, and a snippet block. A theme is ignored: the MJML has no footer.
pub(crate) fn not_convertible(content: &BlocksContent) -> Vec<Issue> {
    let mut issues = Vec::new();
    for section in &content.sections {
        if section.visible_if.is_some() {
            issues.push(
                Issue::new(
                    code::NOT_CONVERTIBLE,
                    block_at(&section.id, Some("visible_if")),
                )
                .with_message("MJML code cannot hold a visibility condition"),
            );
        }
    }
    for (_, block) in blocks_of(content) {
        if matches!(block, Block::Snippet(_)) {
            issues.push(
                Issue::new(code::NOT_CONVERTIBLE, block_at(block.id(), None))
                    .with_message("MJML code cannot hold a snippet"),
            );
        }
        if block.visible_if().is_some() {
            issues.push(
                Issue::new(
                    code::NOT_CONVERTIBLE,
                    block_at(block.id(), Some("visible_if")),
                )
                .with_message("MJML code cannot hold a visibility condition"),
            );
        }
        if let Block::Image(image) = block
            && !matches!(image.src, ImageSrc::Url(_))
        {
            issues.push(
                Issue::new(code::NOT_CONVERTIBLE, block_at(&image.id, Some("src")))
                    .with_message("only an image with a URL converts to MJML code"),
            );
        }
    }
    issues
}

/// The references an image names: an asset needs a URL in the extras, and the theme logo needs a
/// theme whose logo has one. A document with neither input has none to check.
pub(crate) fn check_references(
    content: &BlocksContent,
    extras: &RenderExtras,
    look: &Look<'_>,
    broken: Option<Broken>,
    issues: &mut Vec<Issue>,
) {
    match broken {
        Some(Broken::Theme) => issues.push(
            Issue::new(code::BROKEN_THEME_REFERENCE, doc_at("theme"))
                .with_message("the theme the document names was not passed"),
        ),
        Some(Broken::Footer) => issues.push(
            Issue::new(
                code::BROKEN_SNIPPET_REFERENCE,
                block_at(THEME_FOOTER_ID, Some("snippet_id")),
            )
            .with_message("the theme's footer snippet has no content"),
        ),
        None => {}
    }
    for (_, block) in blocks_of(content) {
        if let Block::Snippet(reference) = block
            && !extras.snippets.contains_key(&reference.snippet_id)
        {
            issues.push(Issue::new(
                code::BROKEN_SNIPPET_REFERENCE,
                block_at(block.id(), Some("snippet_id")),
            ));
        }
    }
    for (base, block) in effective_blocks(content, extras, look.footer) {
        let Block::Image(image) = block else {
            continue;
        };
        let at = nested_at(base.as_deref(), block_at(&image.id, Some("src")));
        match &image.src {
            ImageSrc::Url(_) => {}
            ImageSrc::Asset(id) => {
                if extras.asset_url(id).is_none() {
                    issues.push(Issue::new(code::BROKEN_ASSET_REFERENCE, at));
                }
            }
            ImageSrc::Theme(ThemeSlot::Logo) => {
                if extras.theme_logo_url().is_none() {
                    issues.push(Issue::new(code::BROKEN_THEME_REFERENCE, at));
                }
            }
        }
    }
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
    look: &Look<'_>,
    catalog: &FieldCatalog,
    env: &minijinja::Environment<'static>,
    extras: &RenderExtras,
    issues: &mut Vec<Issue>,
) {
    const AT: &str = "/blocks";
    let values = look.values(&RenderContext::sample(catalog), extras);
    match build(content, look, env, &values, extras, false) {
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
        Err(BuildError::Render(RenderError::InvalidBlocks)) => {
            issues.push(Issue::new(code::MARKUP_IN_PLAIN_FIELD, AT));
        }
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
            let mentions = fields(content, extras, look.footer)
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
    use super::super::snippet::SnippetContent;
    use super::*;
    use crate::content::{content_type_of, validate_document};
    use crate::context::RenderContext;
    use crate::issue::{Severity, severity_of};
    use crate::migrate::read_doc;

    fn newsletter() -> TemplateDocument {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/blocks/newsletter.doc.json");
        read_doc(&fs::read_to_string(path).unwrap())
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
        validate(&doc, &catalog(), &RenderExtras::default())
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
        let issues = validate(&doc, &catalog(), &RenderExtras::default());
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
    fn a_sender_that_is_not_an_identity_id_is_blocking_as_in_a_code_document() {
        for sender in [None, Some("snd_1"), Some("")] {
            let issues = after(|c| c.sender = sender.map(str::to_owned));
            assert_eq!(issues, [entry(code::MISSING_SENDER, "/blocks/sender")]);
        }
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
        doc.schema = 3;
        assert_eq!(
            validate(&doc, &catalog(), &RenderExtras::default())[0].code,
            code::UNSUPPORTED_SCHEMA
        );
        let mut empty = newsletter();
        empty.blocks = None;
        assert_eq!(
            validate(&empty, &catalog(), &RenderExtras::default())[0].code,
            code::MISSING_CONTENT
        );
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
            let issues = validate(&doc, &catalog(), &RenderExtras::default());
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
                    visible_if: None,
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
            [entry(code::MJML_IN_RAW_HTML, "/blocks/block/raw1/html")]
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
                    visible_if: None,
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
                        visible_if: None,
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
                    visible_if: None,
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
                    image.src = ImageSrc::Url(value);
                }
            })
        };
        assert_eq!(
            src("{{ person.name }}"),
            [entry(
                code::URL_FROM_MERGE_FIELD,
                "/blocks/block/hero_image/src"
            )]
        );
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
                src: ImageSrc::Url("https://a.example/i.png".into()),
                alt: " ".into(),
                href: None,
                width: None,
                visible_if: None,
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
                visible_if: None,
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

    #[test]
    fn non_ascii_text_in_and_around_tags_is_validated_and_rendered_without_panicking() {
        let sample = RenderContext::sample(&catalog());
        for (text, rendered) in [
            ("Hej {{ navn og \u{e6}\u{f8}\u{e5}", None),
            ("{{ \u{e6}", None),
            ("{# \u{e6}", None),
            ("{% if x == \u{e9} %}", None),
            ("{% if x == \"\u{e9}\" %}", None),
            ("{{ \"\u{e6}\" }}", Some("\u{e6}")),
            ("\u{e6}{# \u{e6} #}\u{f8}", Some("\u{e6}\u{f8}")),
            (
                "{% if \"\u{e9}\" == \"\u{e9}\" %}\u{e5}{% endif %}",
                Some("\u{e5}"),
            ),
        ] {
            let mut doc = newsletter();
            if let Block::Heading(heading) = block(doc.blocks.as_mut().unwrap(), "intro_title") {
                heading.text = text.into();
            }
            let issues = validate(&doc, &catalog(), &RenderExtras::default());
            let rendered_html = crate::render_email(&doc, &sample, &crate::Hooks::untracked());
            match rendered {
                Some(expected) => {
                    assert!(
                        issues.iter().all(|issue| !issue.is_blocking()),
                        "{text}: {issues:?}"
                    );
                    let html = rendered_html.unwrap().html;
                    assert!(html.contains(&format!(">{expected}<")), "{text}");
                }
                None => {
                    assert!(issues.iter().any(Issue::is_blocking), "{text}: {issues:?}");
                    assert!(rendered_html.is_err(), "{text}");
                }
            }
        }
    }

    #[test]
    fn mjml_structure_in_raw_html_is_blocking_on_its_block() {
        let inject = "</mj-raw></mj-column></mj-section><mj-section><mj-column><mj-text>injected</mj-text></mj-column></mj-section><mj-section><mj-column><mj-raw>";
        let raw = |html: &str| {
            let html = html.to_owned();
            after(move |c| {
                c.sections[1].columns[0].push(Block::RawHtml(RawHtml {
                    id: "raw1".into(),
                    html,
                    visible_if: None,
                }));
            })
        };
        let at = "/blocks/block/raw1/html";
        assert_eq!(raw(inject), [entry(code::MJML_IN_RAW_HTML, at)]);
        for html in [
            "<mj-text>x</mj-text>",
            "<MJ-SECTION></MJ-SECTION>",
            "<x:mj-column></x:mj-column>",
            "</mj-raw>",
            "<p>a</p><mj-button>b</mj-button>",
            "{% if true %}<mj-raw>{% endif %}",
            "<!-- <mj-text> --><p>a</p>",
        ] {
            assert_eq!(raw(html), [entry(code::MJML_IN_RAW_HTML, at)], "{html}");
        }
        // Names that only look like it, and comments, are fine.
        for html in ["<p>mj-text</p>", "<mjx>a</mjx>", "<p>&lt;mj-text&gt;</p>"] {
            assert_eq!(raw(html), Vec::new(), "{html}");
        }
    }

    #[test]
    fn a_value_cannot_bring_mjml_structure_either() {
        // The source is clean; the rendered text of the field is what is checked at render.
        let mut doc = newsletter();
        doc.blocks.as_mut().unwrap().sections[1].columns[0].push(Block::RawHtml(RawHtml {
            id: "raw1".into(),
            html: "{{ person.name | safe }}".into(),
            visible_if: None,
        }));
        let mut context = RenderContext::sample(&catalog());
        context
            .person
            .insert("name".into(), "<mj-text>x</mj-text>".into());
        assert_eq!(
            crate::render_email(&doc, &context, &crate::Hooks::untracked()),
            Err(RenderError::InvalidBlocks)
        );
    }

    #[test]
    fn a_closer_inside_a_string_is_not_a_syntax_error_and_a_real_one_is_blocking() {
        let label = |text: &str| {
            let text = text.to_owned();
            after(move |c| {
                if let Block::Button(button) = block(c, "cta_button") {
                    button.label = text;
                }
            })
        };
        assert_eq!(
            label(r#"{{ person.name | default("}}") }} and {{ "%}" }}"#),
            Vec::new()
        );
        assert_eq!(
            label(r#"{{ person.name | default("}}) }}"#),
            [entry(code::SYNTAX_ERROR, "/blocks/block/cta_button/label")]
        );
        // The rich text that is sent is the sanitized one; its syntax errors are blocking too.
        assert_eq!(
            after(|c| {
                if let Block::Text(rich) = block(c, "intro_text") {
                    rich.content = "<p>{% if person.name %}</p>".into();
                }
            }),
            [entry(
                code::SYNTAX_ERROR,
                "/blocks/block/intro_text/content"
            )]
        );
    }

    #[test]
    fn markup_inside_a_merge_tag_of_rich_text_is_a_blocking_syntax_error() {
        for content in [
            "<p>{{<img src=x onerror=alert(1)>}}</p>",
            "<p>{% <script>x</script> %}</p>",
        ] {
            let issues = after(move |c| {
                if let Block::Text(rich) = block(c, "intro_text") {
                    rich.content = content.into();
                }
            });
            assert_eq!(
                issues,
                [entry(
                    code::SYNTAX_ERROR,
                    "/blocks/block/intro_text/content"
                )],
                "{content}"
            );
        }
    }

    #[test]
    fn markup_in_a_plain_field_is_blocking_when_the_sample_shows_it() {
        for text in [
            r#"{{ "<b>x</b>" | safe }}"#,
            r#"{% autoescape false %}{{ "<i>" }}{% endautoescape %}"#,
        ] {
            assert_eq!(
                after(|c| {
                    if let Block::Heading(heading) = block(c, "intro_title") {
                        heading.text = text.into();
                    }
                }),
                [entry(
                    code::MARKUP_IN_PLAIN_FIELD,
                    "/blocks/block/intro_title/text"
                )],
                "{text}"
            );
        }
        // Literal characters of the text are escaped before the render, so they are not markup.
        assert_eq!(
            after(|c| {
                if let Block::Heading(heading) = block(c, "intro_title") {
                    heading.text = "Fish <&> \"chips\"".into();
                }
            }),
            Vec::new()
        );
        assert_eq!(
            after(|c| c.preheader = r#"{{ "<b>" | safe }}"#.into()),
            [entry(code::MARKUP_IN_PLAIN_FIELD, "/blocks/preheader")]
        );
        // A subject is a header, not markup.
        assert_eq!(after(|c| c.subject = "5 < 6".into()), Vec::new());
    }

    #[test]
    fn a_url_that_starts_with_a_merge_field_warns_that_its_scheme_is_unchecked() {
        let warn = after(|c| {
            if let Block::Button(button) = block(c, "cta_button") {
                button.url = "{{ person.plan }}".into();
            }
            if let Block::Image(image) = block(c, "hero_image") {
                image.href = Some("{{ person.plan }}/x".into());
            }
            if let Block::Text(rich) = block(c, "intro_text") {
                rich.content = "<p><a href=\"{{ person.plan }}\">x</a></p>".into();
            }
        });
        assert_eq!(
            warn,
            [
                entry(code::URL_FROM_MERGE_FIELD, "/blocks/block/hero_image/href"),
                entry(
                    code::URL_FROM_MERGE_FIELD,
                    "/blocks/block/intro_text/content"
                ),
                entry(code::URL_FROM_MERGE_FIELD, "/blocks/block/cta_button/url"),
            ]
        );
        assert_eq!(severity_of(code::URL_FROM_MERGE_FIELD), Severity::Warning);
        // The system's own signed URLs are not a risk.
        assert_eq!(
            after(|c| {
                if let Block::Button(button) = block(c, "cta_button") {
                    button.url = "{{ system.preferences_url }}".into();
                }
            }),
            Vec::new()
        );
    }

    fn newsletter_with(edit: impl FnOnce(&mut serde_json::Value)) -> TemplateDocument {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/blocks/newsletter.doc.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        edit(&mut value);
        read_doc(&value.to_string())
    }

    fn codes_with(doc: &TemplateDocument, extras: &RenderExtras) -> Vec<(&'static str, String)> {
        validate(doc, &catalog(), extras)
            .into_iter()
            .filter(Issue::is_blocking)
            .map(|issue| (issue.code, issue.at))
            .collect()
    }

    const ASSET: &str = "0192b1a4-7c3e-7000-8000-00000000b001";

    #[test]
    fn asset_and_theme_references_must_resolve() {
        let asset = newsletter_with(|v| {
            v["blocks"]["sections"][0]["columns"][0][0]["src"] =
                serde_json::json!({"asset": ASSET});
        });
        assert_eq!(
            codes_with(&asset, &RenderExtras::default()),
            [(
                "broken_asset_reference",
                "/blocks/block/hero_image/src".to_owned()
            )]
        );
        let mut extras = RenderExtras::default();
        extras
            .assets
            .insert(ASSET.into(), "https://cdn.example/a.png".into());
        assert_eq!(codes_with(&asset, &extras), Vec::new());

        let logo = newsletter_with(|v| {
            v["blocks"]["sections"][0]["columns"][0][0]["src"] =
                serde_json::json!({"theme": "logo"});
        });
        assert_eq!(
            codes_with(&logo, &extras),
            [(
                "broken_theme_reference",
                "/blocks/block/hero_image/src".to_owned()
            )]
        );
        extras.theme = Some(crate::theme::ThemeTokens {
            logo: Some(ASSET.into()),
            ..Default::default()
        });
        assert_eq!(codes_with(&logo, &extras), Vec::new());
    }

    const FOOTER: &str = "0192b1a4-7c3e-7000-8000-00000000c001";
    const PROMO: &str = "0192b1a4-7c3e-7000-8000-00000000c002";
    const THEME_ID: &str = "0192b1a4-7c3e-7000-8000-00000000d001";

    fn snippet_of(value: serde_json::Value) -> SnippetContent {
        serde_json::from_value(value).unwrap()
    }

    fn unsubscribe_snippet() -> SnippetContent {
        snippet_of(serde_json::json!({"blocks": [{
            "type": "text",
            "id": "legal",
            "content": "<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"
        }]}))
    }

    fn text_snippet(content: &str) -> SnippetContent {
        snippet_of(serde_json::json!({"blocks": [{
            "type": "text",
            "id": "legal",
            "content": content
        }]}))
    }

    fn themed_extras(footer: Option<SnippetContent>) -> RenderExtras {
        let mut extras = RenderExtras {
            theme: Some(crate::theme::ThemeTokens {
                footer_snippet: footer.as_ref().map(|_| FOOTER.to_owned()),
                ..Default::default()
            }),
            theme_name: Some("Brand".into()),
            ..Default::default()
        };
        if let Some(footer) = footer {
            extras.snippets.insert(FOOTER.into(), footer);
        }
        extras
    }

    /// The newsletter with its own footer section removed and the theme footer switched on.
    fn footerless(footer: &str) -> TemplateDocument {
        newsletter_with(|v| {
            v["blocks"]["theme"] = serde_json::json!("default");
            v["blocks"]["footer"] = serde_json::json!(footer);
            if let Some(sections) = v["blocks"]["sections"].as_array_mut() {
                sections.retain(|section| section["id"] != "footer");
            }
        })
    }

    #[test]
    fn a_snippet_block_needs_content_and_renders_from_it_in_a_section_column() {
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][1]["columns"][0]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "snippet", "id": "sn", "snippet_id": PROMO}));
        });
        assert_eq!(
            codes_with(&doc, &RenderExtras::default()),
            [(
                "broken_snippet_reference",
                "/blocks/block/sn/snippet_id".to_owned()
            )]
        );
        let mut extras = RenderExtras::default();
        extras
            .snippets
            .insert(PROMO.into(), text_snippet("<p>Spring</p>"));
        assert_eq!(codes_with(&doc, &extras), Vec::new());
        assert!(crate::blocks_to_mjml(doc.blocks.as_ref().unwrap()).is_err());
    }

    #[test]
    fn an_unknown_merge_field_in_a_snippet_is_blocking_and_the_path_names_the_snippet_block() {
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][1]["columns"][0]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "snippet", "id": "sn", "snippet_id": PROMO}));
        });
        let mut extras = RenderExtras::default();
        extras
            .snippets
            .insert(PROMO.into(), text_snippet("<p>{{ person.nope }}</p>"));
        let issues = validate(&doc, &catalog(), &extras);
        let blocking: Vec<_> = issues.iter().filter(|issue| issue.is_blocking()).collect();
        assert_eq!(blocking.len(), 1, "{issues:?}");
        assert_eq!(blocking[0].code, code::UNKNOWN_MERGE_FIELD);
        assert_eq!(blocking[0].at, "/blocks/block/sn/block/legal/content");
        assert_eq!(blocking[0].field.as_deref(), Some("person.nope"));
    }

    #[test]
    fn a_snippet_that_is_not_a_valid_snippet_is_blocking_where_it_is_used() {
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][1]["columns"][0]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "snippet", "id": "sn", "snippet_id": PROMO}));
        });
        let mut extras = RenderExtras::default();
        extras.snippets.insert(
            PROMO.into(),
            snippet_of(serde_json::json!({"blocks": [
                {"type": "snippet", "id": "inner", "snippet_id": FOOTER},
                {"type": "spacer", "id": "gap", "height": 10, "visible_if": {"x": 1}}
            ]})),
        );
        assert_eq!(
            codes_with(&doc, &extras),
            [
                ("snippet_nested", "/blocks/block/sn/block/inner".to_owned()),
                (
                    "snippet_condition",
                    "/blocks/block/sn/block/gap/visible_if".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_marketing_document_whose_snippets_have_no_unsubscribe_link_is_blocking() {
        let doc = newsletter_with(|v| {
            if let Some(sections) = v["blocks"]["sections"].as_array_mut() {
                sections.retain(|section| section["id"] != "footer");
                sections.push(serde_json::json!({"id": "tail", "columns": [[
                    {"type": "snippet", "id": "sn", "snippet_id": PROMO}
                ]]}));
            }
        });
        let mut extras = RenderExtras::default();
        extras
            .snippets
            .insert(PROMO.into(), text_snippet("<p>No link</p>"));
        assert_eq!(
            codes_with(&doc, &extras),
            [("missing_unsubscribe", "/blocks".to_owned())]
        );
        extras.snippets.insert(PROMO.into(), unsubscribe_snippet());
        assert_eq!(codes_with(&doc, &extras), Vec::new());
    }

    #[test]
    fn the_theme_footer_supplies_the_unsubscribe_link_of_a_marketing_document() {
        let doc = footerless("theme");
        assert_eq!(
            codes_with(&doc, &themed_extras(Some(unsubscribe_snippet()))),
            Vec::new()
        );
        assert_eq!(
            codes_with(&doc, &themed_extras(Some(text_snippet("<p>No link</p>")))),
            [("missing_unsubscribe", "/blocks".to_owned())]
        );
    }

    #[test]
    fn an_opted_out_document_with_no_link_of_its_own_gets_missing_unsubscribe() {
        let extras = themed_extras(Some(unsubscribe_snippet()));
        assert_eq!(
            codes_with(&footerless("none"), &extras),
            [("missing_unsubscribe", "/blocks".to_owned())]
        );
        let theme_none = newsletter_with(|v| {
            if let Some(sections) = v["blocks"]["sections"].as_array_mut() {
                sections.retain(|section| section["id"] != "footer");
            }
        });
        assert_eq!(
            codes_with(&theme_none, &extras),
            [("missing_unsubscribe", "/blocks".to_owned())]
        );
    }

    #[test]
    fn an_unknown_merge_field_in_the_theme_footer_is_blocking_and_names_theme_footer() {
        let extras = themed_extras(Some(text_snippet(
            "<p>{{ person.nope }} <a href=\"{{ system.unsubscribe_url }}\">u</a></p>",
        )));
        let issues = validate(&footerless("theme"), &catalog(), &extras);
        let blocking: Vec<_> = issues.iter().filter(|issue| issue.is_blocking()).collect();
        assert_eq!(blocking.len(), 1, "{issues:?}");
        assert_eq!(blocking[0].code, code::UNKNOWN_MERGE_FIELD);
        assert_eq!(
            blocking[0].at,
            "/blocks/block/theme-footer/block/legal/content"
        );
    }

    #[test]
    fn a_named_theme_or_a_footer_snippet_that_was_not_passed_is_a_broken_reference() {
        let named = newsletter_with(|v| {
            v["blocks"]["theme"] = serde_json::json!({"id": THEME_ID});
        });
        assert_eq!(
            codes_with(&named, &RenderExtras::default()),
            [("broken_theme_reference", "/blocks/theme".to_owned())]
        );
        assert_eq!(
            codes_with(&named, &themed_extras(Some(unsubscribe_snippet()))),
            Vec::new()
        );

        let mut extras = themed_extras(Some(unsubscribe_snippet()));
        extras.snippets.clear();
        assert_eq!(
            codes_with(&footerless("theme"), &extras),
            [(
                "broken_snippet_reference",
                "/blocks/block/theme-footer/snippet_id".to_owned()
            )]
        );
    }

    #[test]
    fn a_followed_default_with_no_theme_passed_is_not_a_broken_reference() {
        let doc = newsletter_with(|v| v["blocks"]["theme"] = serde_json::json!("default"));
        assert_eq!(codes_with(&doc, &RenderExtras::default()), Vec::new());
    }

    #[test]
    fn theme_values_are_known_names_only_while_a_theme_is_in_effect() {
        let with = |theme: &str, extras: &RenderExtras, text: &str| {
            let doc = newsletter_with(|v| {
                v["blocks"]["theme"] = serde_json::json!(theme);
                v["blocks"]["sections"][1]["columns"][0][0]["text"] = serde_json::json!(text);
            });
            validate(&doc, &catalog(), extras)
                .into_iter()
                .filter(Issue::is_blocking)
                .map(|issue| (issue.code, issue.field))
                .collect::<Vec<_>>()
        };
        let extras = themed_extras(None);
        for name in crate::context::THEME_FIELDS {
            let text = format!("{{{{ theme.{name} | default(\"\") }}}}");
            assert_eq!(with("default", &extras, &text), Vec::new(), "{name}");
        }
        assert_eq!(
            with("default", &extras, "{{ theme.nope }}"),
            [(code::UNKNOWN_MERGE_FIELD, Some("theme.nope".to_owned()))]
        );
        // No theme in effect: every `theme.*` name is unknown, with a theme in the extras or not.
        for (theme, extras) in [("none", &extras), ("default", &RenderExtras::default())] {
            assert_eq!(
                with(theme, extras, "{{ theme.primary }}"),
                [(code::UNKNOWN_MERGE_FIELD, Some("theme.primary".to_owned()))]
            );
        }
    }

    #[test]
    fn the_footer_id_is_reserved_and_a_document_that_uses_it_gets_the_duplicate_id_issue() {
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][0]["id"] = serde_json::json!("theme-footer");
        });
        let issues = codes_with(&doc, &RenderExtras::default());
        assert_eq!(
            issues,
            [(
                "duplicate_block_id",
                "/blocks/block/theme-footer".to_owned()
            )]
        );
    }

    #[test]
    fn a_theme_logo_inside_a_snippet_resolves_against_the_theme_in_effect() {
        let doc = newsletter_with(|v| {
            v["blocks"]["theme"] = serde_json::json!("default");
            v["blocks"]["sections"][1]["columns"][0]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "snippet", "id": "sn", "snippet_id": PROMO}));
        });
        let mut extras = RenderExtras::default();
        extras.snippets.insert(
            PROMO.into(),
            snippet_of(serde_json::json!({"blocks": [
                {"type": "image", "id": "logo", "src": {"theme": "logo"}, "alt": "Logo"}
            ]})),
        );
        assert_eq!(
            codes_with(&doc, &extras),
            [(
                "broken_theme_reference",
                "/blocks/block/sn/block/logo/src".to_owned()
            )]
        );
    }

    #[test]
    fn a_condition_is_bounded_in_size_and_depth_and_listed_in_order() {
        let big = serde_json::json!({"v": "x".repeat(17 * 1024)});
        let mut deep = serde_json::json!(1);
        for _ in 0..40 {
            deep = serde_json::json!([deep]);
        }
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][0]["visible_if"] = big.clone();
            v["blocks"]["sections"][1]["columns"][0][0]["visible_if"] = deep.clone();
            v["blocks"]["sections"][1]["columns"][0][1]["visible_if"] =
                serde_json::json!({"ok": 1});
        });
        let issues = codes_with(&doc, &RenderExtras::default());
        let too_large: Vec<_> = issues
            .iter()
            .filter(|(code, _)| *code == "condition_too_large")
            .collect();
        assert_eq!(
            too_large,
            [
                &(
                    "condition_too_large",
                    "/blocks/block/hero/visible_if".to_owned()
                ),
                &(
                    "condition_too_large",
                    "/blocks/block/intro_title/visible_if".to_owned()
                ),
            ]
        );
        let ids: Vec<&str> = crate::conditions(doc.blocks.as_ref().unwrap())
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["hero", "intro_title", "intro_text"]);
    }

    #[test]
    fn a_condition_on_a_section_or_block_blocks_publishing_for_now() {
        let none = newsletter_with(|_| {});
        assert_eq!(codes_with(&none, &RenderExtras::default()), Vec::new());
        let doc = newsletter_with(|v| {
            v["blocks"]["sections"][0]["visible_if"] = serde_json::json!({"all": []});
            v["blocks"]["sections"][1]["columns"][0][1]["visible_if"] =
                serde_json::json!({"all": []});
        });
        assert_eq!(
            codes_with(&doc, &RenderExtras::default()),
            [
                (
                    "condition_unsupported",
                    "/blocks/block/hero/visible_if".to_owned()
                ),
                (
                    "condition_unsupported",
                    "/blocks/block/intro_text/visible_if".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_document_with_a_condition_or_an_asset_image_does_not_convert_to_mjml() {
        let conditioned = newsletter_with(|v| {
            v["blocks"]["sections"][1]["visible_if"] = serde_json::json!({"ok": 1});
        });
        let error = crate::blocks_to_mjml(conditioned.blocks.as_ref().unwrap()).unwrap_err();
        assert_eq!(error.issues[0].code, code::NOT_CONVERTIBLE);
        let asset = newsletter_with(|v| {
            v["blocks"]["sections"][0]["columns"][0][0]["src"] =
                serde_json::json!({"asset": ASSET});
        });
        assert!(crate::blocks_to_mjml(asset.blocks.as_ref().unwrap()).is_err());
    }
}
