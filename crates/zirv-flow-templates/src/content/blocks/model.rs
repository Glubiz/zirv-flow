//! The block document v1 (architecture 11.1, 11.2): the types, their limits and the checks of
//! their shape that need no template engine.
//!
//! Every struct refuses a field it does not know (`deny_unknown_fields`), and the block enum
//! refuses a type it does not know, so a document a newer editor wrote is an error here and never
//! silently loses content. [`BlocksError::from_serde`] names which of the two it was.

use std::fmt;

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use zirv_flow_core::segment::pointer_push;

use crate::document::Category;
use crate::issue::{Issue, code};

/// Sections in one document.
pub const MAX_SECTIONS: usize = 50;
/// Blocks in one document, over every column of every section.
pub const MAX_BLOCKS: usize = 500;
/// Columns in one section.
pub const MAX_COLUMNS: usize = 3;
/// Bytes of a block id.
pub const MAX_ID_BYTES: usize = 64;
/// Bytes of a subject or preheader source.
pub const MAX_SUBJECT_SOURCE_BYTES: usize = 2_000;
/// Bytes of a heading, button label or alt text source.
pub const MAX_LABEL_BYTES: usize = 1_000;
/// Bytes of a URL source.
pub const MAX_URL_BYTES: usize = 2_048;
/// Bytes of a rich text source.
pub const MAX_RICH_TEXT_BYTES: usize = 50_000;
/// Bytes of a raw HTML source.
pub const MAX_RAW_HTML_BYTES: usize = 100_000;
/// Bytes of all the text sources of a document together.
pub const MAX_DOCUMENT_BYTES: usize = 400_000;
/// The narrowest content width, in pixels.
pub const MIN_CONTENT_WIDTH: u32 = 480;
/// The widest content width, in pixels.
pub const MAX_CONTENT_WIDTH: u32 = 800;
/// The tallest spacer, in pixels.
pub const MAX_SPACER_HEIGHT: u32 = 200;
/// The widest image, in pixels.
pub const MAX_IMAGE_WIDTH: u32 = 800;
/// Characters of a subject beyond which a phone cuts it off.
pub const SUBJECT_DISPLAY_CHARS: usize = 78;
/// Characters of a preheader beyond which an inbox cuts it off.
pub const PREHEADER_DISPLAY_CHARS: usize = 110;

/// The web-safe font stacks a document may name, with the CSS each stands for.
pub const FONT_STACKS: [(&str, &str); 8] = [
    ("arial", "Arial, Helvetica, sans-serif"),
    ("helvetica", "Helvetica, Arial, sans-serif"),
    ("verdana", "Verdana, Geneva, sans-serif"),
    ("tahoma", "Tahoma, Geneva, sans-serif"),
    ("trebuchet", "'Trebuchet MS', Helvetica, sans-serif"),
    ("georgia", "Georgia, 'Times New Roman', serif"),
    ("times", "'Times New Roman', Times, serif"),
    ("courier", "'Courier New', Courier, monospace"),
];

/// The CSS of a font stack name, if it is one of [`FONT_STACKS`].
pub(crate) fn font_css(name: &str) -> Option<&'static str> {
    FONT_STACKS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, css)| *css)
}

/// The content of a block email: the envelope fields, the style settings and the section list.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlocksContent {
    /// A MiniJinja template.
    pub subject: String,
    /// A MiniJinja template; the text an inbox shows after the subject.
    #[serde(default)]
    pub preheader: String,
    /// The sender identity id; resolved by the send pipeline, never rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default)]
    pub category: Category,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub sections: Vec<Section>,
}

impl BlocksContent {
    /// A content with the given subject and no sections.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            preheader: String::new(),
            sender: None,
            lang: None,
            category: Category::default(),
            settings: Settings::default(),
            sections: Vec::new(),
        }
    }
}

/// The per-document style settings that stand in for a brand theme until M7.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `#rgb` or `#rrggbb`.
    pub background_color: String,
    /// Pixels, 480 to 800.
    pub content_width: u32,
    /// A name from [`FONT_STACKS`].
    pub font_family: String,
    pub text_color: String,
    pub link_color: String,
    pub button_color: String,
    pub button_text_color: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            background_color: "#ffffff".to_owned(),
            content_width: 600,
            font_family: "arial".to_owned(),
            text_color: "#222222".to_owned(),
            link_color: "#1a5fb4".to_owned(),
            button_color: "#1a5fb4".to_owned(),
            button_text_color: "#ffffff".to_owned(),
        }
    }
}

/// A row of one to three columns.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<String>,
    /// One to four lengths in pixels, such as `20px 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<String>,
    /// Each column is a list of blocks. A section holds no section.
    pub columns: Vec<Vec<Block>>,
}

/// How text or a button sits in its column.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    pub(crate) const fn css(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }
}

/// One block. The `type` field names the variant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Heading(Heading),
    Text(RichText),
    Image(Image),
    Button(Button),
    Divider(Divider),
    Spacer(Spacer),
    RawHtml(RawHtml),
}

impl Block {
    /// The stable id of the block.
    pub fn id(&self) -> &str {
        match self {
            Self::Heading(block) => &block.id,
            Self::Text(block) => &block.id,
            Self::Image(block) => &block.id,
            Self::Button(block) => &block.id,
            Self::Divider(block) => &block.id,
            Self::Spacer(block) => &block.id,
            Self::RawHtml(block) => &block.id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Heading {
    pub id: String,
    /// 1 to 3.
    pub level: u8,
    /// A MiniJinja template, plain text.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RichText {
    pub id: String,
    /// A MiniJinja template that renders to the rich text subset of HTML.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub id: String,
    /// An `https` URL, or a merge tag that renders to one.
    pub src: String,
    #[serde(default)]
    pub alt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// Pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub id: String,
    pub label: String,
    pub url: String,
    #[serde(default)]
    pub align: Align,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Divider {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Spacer {
    pub id: String,
    /// Pixels, 1 to 200.
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawHtml {
    pub id: String,
    /// The marketer's own HTML, a MiniJinja template, passed through as `mj-raw`.
    pub html: String,
}

/// Why a block document could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlocksError {
    /// The `schema` is not the one this crate reads.
    UnsupportedSchema,
    /// A block has a `type` that is not in the v1 set.
    UnknownBlock,
    /// An object has a field that the v1 document does not define.
    UnknownField,
    /// The value is not a block document in any other way.
    Malformed,
}

impl BlocksError {
    /// A fixed machine-readable code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedSchema => "unsupported_schema",
            Self::UnknownBlock => "unknown_block",
            Self::UnknownField => "unknown_field",
            Self::Malformed => "malformed_document",
        }
    }

    /// Classify the error of `serde_json` on a block document. The message texts are serde's
    /// own (`unknown variant` for a block type, `unknown field` for a field); a test pins them.
    pub fn from_serde(error: &serde_json::Error) -> Self {
        let text = error.to_string();
        if text.contains("unknown variant") {
            Self::UnknownBlock
        } else if text.contains("unknown field") {
            Self::UnknownField
        } else {
            Self::Malformed
        }
    }
}

impl fmt::Display for BlocksError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedSchema => "the document schema is not supported",
            Self::UnknownBlock => "a block has a type that is not part of the v1 block set",
            Self::UnknownField => "an object has a field the v1 block document does not define",
            Self::Malformed => "the value is not a block document",
        })
    }
}

impl std::error::Error for BlocksError {}

/// Read a template document that holds block content from JSON, with typed errors: a `schema`
/// other than the one this crate reads is [`BlocksError::UnsupportedSchema`] before anything
/// else is looked at.
pub fn parse_blocks_document(
    value: &serde_json::Value,
) -> Result<crate::document::TemplateDocument, BlocksError> {
    let schema = value.get("schema").and_then(serde_json::Value::as_u64);
    if schema != Some(u64::from(crate::document::SCHEMA_VERSION)) {
        return Err(BlocksError::UnsupportedSchema);
    }
    serde_json::from_value(value.clone()).map_err(|error| BlocksError::from_serde(&error))
}

/// `#rgb` or `#rrggbb`.
pub(crate) fn is_color(text: &str) -> bool {
    let Some(digits) = text.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6) && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One to four non-negative lengths in whole pixels (`0`, `20px`, `10px 25px`).
pub(crate) fn is_padding(text: &str) -> bool {
    let parts: Vec<&str> = text.split(' ').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            let digits = part.strip_suffix("px").unwrap_or(part);
            !digits.is_empty()
                && digits.len() <= 3
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// A block id: ASCII letters, digits, `_` and `-`, at most [`MAX_ID_BYTES`] long.
pub(crate) fn is_block_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_ID_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// A language tag as `lang` attributes take it: letters, digits and `-`.
pub(crate) fn is_lang(text: &str) -> bool {
    (2..=35).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The path of a field of the document itself (`subject`, `settings/link_color`).
pub(crate) fn doc_at(field: &str) -> String {
    format!("/blocks/{field}")
}

/// The path of a block, or of one of its fields.
pub(crate) fn block_at(id: &str, field: Option<&str>) -> String {
    let base = pointer_push("/blocks/block", id);
    match field {
        Some(field) => format!("{base}/{field}"),
        None => base,
    }
}

/// Every block with the section it sits in, in document order.
pub(crate) fn blocks_of(content: &BlocksContent) -> impl Iterator<Item = (&Section, &Block)> {
    content.sections.iter().flat_map(|section| {
        section
            .columns
            .iter()
            .flatten()
            .map(move |block| (section, block))
    })
}

/// The checks of the shape of the document: counts, ids, columns, settings and the numbers and
/// colours of the blocks. Text and URL fields are checked where they are rendered. The result
/// is in document order; a document over a count limit gets that issue alone.
pub(crate) fn check_shape(content: &BlocksContent) -> Vec<Issue> {
    let mut issues = Vec::new();
    if content.sections.len() > MAX_SECTIONS {
        issues.push(
            Issue::new(code::BLOCKS_LIMIT, doc_at("sections"))
                .with_message(format!("at most {MAX_SECTIONS} sections")),
        );
    }
    let total: usize = content
        .sections
        .iter()
        .map(|section| section.columns.iter().map(Vec::len).sum::<usize>())
        .sum();
    if total > MAX_BLOCKS {
        issues.push(
            Issue::new(code::BLOCKS_LIMIT, doc_at("sections"))
                .with_message(format!("at most {MAX_BLOCKS} blocks")),
        );
    }
    if !issues.is_empty() {
        return issues;
    }

    check_settings(&content.settings, &mut issues);
    if let Some(lang) = content
        .lang
        .as_deref()
        .filter(|lang| !lang.trim().is_empty())
        && !is_lang(lang)
    {
        issues.push(
            Issue::new(code::INVALID_SETTING, doc_at("lang"))
                .with_message("a language tag such as `en` or `da-DK`"),
        );
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for section in &content.sections {
        let at = |field: &str| block_at(&section.id, Some(field));
        check_id(&section.id, &mut seen, &mut issues);
        if section.columns.is_empty() || section.columns.len() > MAX_COLUMNS {
            issues.push(
                Issue::new(code::INVALID_COLUMNS, at("columns"))
                    .with_message(format!("a section has 1 to {MAX_COLUMNS} columns")),
            );
        }
        if let Some(color) = &section.background_color
            && !is_color(color)
        {
            issues.push(color_issue(at("background_color")));
        }
        if let Some(padding) = &section.padding
            && !is_padding(padding)
        {
            issues.push(
                Issue::new(code::INVALID_SETTING, at("padding"))
                    .with_message("one to four lengths in pixels, such as `20px 0`"),
            );
        }
        for block in section.columns.iter().flatten() {
            check_id(block.id(), &mut seen, &mut issues);
            check_block_numbers(block, &mut issues);
        }
    }
    issues
}

fn color_issue(at: String) -> Issue {
    Issue::new(code::INVALID_SETTING, at).with_message("a colour as `#rgb` or `#rrggbb`")
}

fn check_id<'a>(id: &'a str, seen: &mut HashSet<&'a str>, issues: &mut Vec<Issue>) {
    if !is_block_id(id) {
        issues.push(
            Issue::new(code::INVALID_BLOCK_ID, block_at(id, None)).with_message(format!(
                "an id is 1 to {MAX_ID_BYTES} letters, digits, `_` or `-`"
            )),
        );
    } else if !seen.insert(id) {
        issues.push(Issue::new(code::DUPLICATE_BLOCK_ID, block_at(id, None)));
    }
}

fn check_settings(settings: &Settings, issues: &mut Vec<Issue>) {
    for (field, value) in [
        ("background_color", &settings.background_color),
        ("text_color", &settings.text_color),
        ("link_color", &settings.link_color),
        ("button_color", &settings.button_color),
        ("button_text_color", &settings.button_text_color),
    ] {
        if !is_color(value) {
            issues.push(color_issue(doc_at(&format!("settings/{field}"))));
        }
    }
    if !(MIN_CONTENT_WIDTH..=MAX_CONTENT_WIDTH).contains(&settings.content_width) {
        issues.push(
            Issue::new(code::INVALID_SETTING, doc_at("settings/content_width"))
                .with_message(format!("{MIN_CONTENT_WIDTH} to {MAX_CONTENT_WIDTH} pixels")),
        );
    }
    if font_css(&settings.font_family).is_none() {
        let names: Vec<&str> = FONT_STACKS.iter().map(|(name, _)| *name).collect();
        issues.push(
            Issue::new(code::INVALID_SETTING, doc_at("settings/font_family"))
                .with_message(format!("one of {}", names.join(", "))),
        );
    }
}

fn check_block_numbers(block: &Block, issues: &mut Vec<Issue>) {
    match block {
        Block::Heading(heading) if !(1..=3).contains(&heading.level) => issues.push(
            Issue::new(code::INVALID_SETTING, block_at(&heading.id, Some("level")))
                .with_message("a level from 1 to 3"),
        ),
        Block::Image(image)
            if image
                .width
                .is_some_and(|width| !(1..=MAX_IMAGE_WIDTH).contains(&width)) =>
        {
            issues.push(
                Issue::new(code::INVALID_SETTING, block_at(&image.id, Some("width")))
                    .with_message(format!("1 to {MAX_IMAGE_WIDTH} pixels")),
            );
        }
        Block::Divider(divider)
            if divider
                .color
                .as_deref()
                .is_some_and(|color| !is_color(color)) =>
        {
            issues.push(color_issue(block_at(&divider.id, Some("color"))));
        }
        Block::Spacer(spacer) if !(1..=MAX_SPACER_HEIGHT).contains(&spacer.height) => issues.push(
            Issue::new(code::INVALID_SETTING, block_at(&spacer.id, Some("height")))
                .with_message(format!("1 to {MAX_SPACER_HEIGHT} pixels")),
        ),
        _ => {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    pub(crate) fn heading(id: &str) -> Block {
        Block::Heading(Heading {
            id: id.into(),
            level: 1,
            text: "Hi".into(),
            align: None,
        })
    }

    fn content_with(columns: Vec<Vec<Block>>) -> BlocksContent {
        let mut content = BlocksContent::new("Hi");
        content.sections.push(Section {
            id: "s1".into(),
            background_color: None,
            padding: None,
            columns,
        });
        content
    }

    fn at_codes(issues: &[Issue]) -> Vec<(&'static str, String)> {
        issues
            .iter()
            .map(|issue| (issue.code, issue.at.clone()))
            .collect()
    }

    #[test]
    fn a_valid_shape_has_no_issue() {
        let content = content_with(vec![vec![heading("a")], vec![heading("b")]]);
        assert_eq!(check_shape(&content), Vec::new());
    }

    #[test]
    fn an_unknown_block_type_and_an_unknown_field_are_typed_errors() {
        let doc = |blocks: serde_json::Value| {
            json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
                "subject": "Hi", "sections": [{"id": "s", "columns": [blocks]}]}})
        };
        let unknown_block = doc(json!([{"type": "carousel", "id": "x"}]));
        assert_eq!(
            parse_blocks_document(&unknown_block).unwrap_err(),
            BlocksError::UnknownBlock
        );
        let unknown_field = doc(json!([{"type": "divider", "id": "x", "style": "dotted"}]));
        assert_eq!(
            parse_blocks_document(&unknown_field).unwrap_err(),
            BlocksError::UnknownField
        );
        let mut unknown_content_field = doc(json!([]));
        unknown_content_field["blocks"]["theme"] = json!("dark");
        assert_eq!(
            parse_blocks_document(&unknown_content_field).unwrap_err(),
            BlocksError::UnknownField
        );
        let missing_field = doc(json!([{"type": "heading", "id": "x"}]));
        assert_eq!(
            parse_blocks_document(&missing_field).unwrap_err(),
            BlocksError::Malformed
        );
        for error in [
            BlocksError::UnknownBlock,
            BlocksError::UnknownField,
            BlocksError::Malformed,
            BlocksError::UnsupportedSchema,
        ] {
            assert!(!error.to_string().is_empty());
            assert!(!error.code().is_empty());
        }
    }

    #[test]
    fn schema_one_is_read_and_another_schema_is_refused_first() {
        let mut doc = json!({"schema": 1, "channel": "email", "mode": "blocks",
                             "blocks": {"subject": "Hi"}});
        let read = parse_blocks_document(&doc).unwrap();
        assert_eq!(read.blocks, Some(BlocksContent::new("Hi")));
        for schema in [json!(2), json!(0), json!("1"), json!(null)] {
            doc["schema"] = schema;
            assert_eq!(
                parse_blocks_document(&doc).unwrap_err(),
                BlocksError::UnsupportedSchema
            );
        }
        // A newer schema may also have a shape this one cannot read: the schema still wins.
        let newer = json!({"schema": 2, "blocks": {"nope": true}});
        assert_eq!(
            parse_blocks_document(&newer).unwrap_err(),
            BlocksError::UnsupportedSchema
        );
    }

    #[test]
    fn a_block_document_round_trips_through_json() {
        let mut content = content_with(vec![vec![
            heading("h"),
            Block::Spacer(Spacer {
                id: "sp".into(),
                height: 10,
            }),
        ]]);
        content.sender = Some("snd".into());
        content.lang = Some("en".into());
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(
            serde_json::from_str::<BlocksContent>(&json).unwrap(),
            content
        );
    }

    #[test]
    fn counts_over_the_limits_are_blocking_and_stop_the_other_checks() {
        let mut content = BlocksContent::new("Hi");
        for index in 0..=MAX_SECTIONS {
            content.sections.push(Section {
                id: format!("s{index}"),
                background_color: Some("red".into()),
                padding: None,
                columns: vec![vec![]],
            });
        }
        let issues = check_shape(&content);
        assert_eq!(
            at_codes(&issues),
            [(code::BLOCKS_LIMIT, "/blocks/sections".to_owned())]
        );
        assert!(issues[0].is_blocking());

        let many: Vec<Block> = (0..=MAX_BLOCKS)
            .map(|n| heading(&format!("b{n}")))
            .collect();
        let issues = check_shape(&content_with(vec![many]));
        assert_eq!(
            at_codes(&issues),
            [(code::BLOCKS_LIMIT, "/blocks/sections".to_owned())]
        );
    }

    #[test]
    fn ids_must_be_well_formed_and_unique_across_sections_and_blocks() {
        let content = content_with(vec![vec![heading("s1"), heading("a b"), heading("c")]]);
        let mut twice = content.clone();
        twice.sections[0].columns[0].push(heading("c"));
        assert_eq!(
            at_codes(&check_shape(&content)),
            [
                (code::DUPLICATE_BLOCK_ID, "/blocks/block/s1".to_owned()),
                (code::INVALID_BLOCK_ID, "/blocks/block/a b".to_owned()),
            ]
        );
        assert_eq!(
            at_codes(&check_shape(&twice))[2],
            (code::DUPLICATE_BLOCK_ID, "/blocks/block/c".to_owned())
        );
        assert!(!is_block_id(""));
        assert!(!is_block_id(&"x".repeat(MAX_ID_BYTES + 1)));
        assert!(is_block_id(&"x".repeat(MAX_ID_BYTES)));
    }

    #[test]
    fn a_section_has_one_to_three_columns() {
        for (columns, ok) in [(0, false), (1, true), (3, true), (4, false)] {
            let content = content_with(vec![vec![]; columns]);
            let issues = check_shape(&content);
            assert_eq!(issues.is_empty(), ok, "{columns}");
            if !ok {
                assert_eq!(issues[0].at, "/blocks/block/s1/columns");
                assert_eq!(issues[0].code, code::INVALID_COLUMNS);
            }
        }
    }

    #[test]
    fn settings_are_checked_with_their_paths() {
        let mut content = content_with(vec![vec![]]);
        content.settings.background_color = "white".into();
        content.settings.text_color = "#12".into();
        content.settings.link_color = "#12345g".into();
        content.settings.content_width = 479;
        content.settings.font_family = "comic".into();
        content.lang = Some("en us".into());
        let at: Vec<String> = check_shape(&content).into_iter().map(|i| i.at).collect();
        assert_eq!(
            at,
            [
                "/blocks/settings/background_color",
                "/blocks/settings/text_color",
                "/blocks/settings/link_color",
                "/blocks/settings/content_width",
                "/blocks/settings/font_family",
                "/blocks/lang",
            ]
        );
        for width in [480, 600, 800] {
            content.settings = Settings {
                content_width: width,
                ..Settings::default()
            };
            content.lang = Some("da-DK".into());
            assert_eq!(check_shape(&content), Vec::new(), "{width}");
        }
        content.settings.content_width = 801;
        assert_eq!(check_shape(&content).len(), 1);
    }

    #[test]
    fn colours_paddings_and_numbers_are_checked() {
        for good in ["#fff", "#FFFFFF", "#0a1B2c"] {
            assert!(is_color(good), "{good}");
        }
        for bad in [
            "fff",
            "#ffff",
            "#ggg",
            "#",
            "rgb(1,2,3)",
            "#fff ",
            "#ffffffff",
        ] {
            assert!(!is_color(bad), "{bad}");
        }
        for good in ["0", "20px 0", "10px 25px 0 5px", "0px"] {
            assert!(is_padding(good), "{good}");
        }
        for bad in ["", "1em", "1 2 3 4 5", "-1px", "px", "20px  0", "1000px"] {
            assert!(!is_padding(bad), "{bad}");
        }
        let mut content = content_with(vec![vec![
            Block::Heading(Heading {
                id: "h".into(),
                level: 4,
                text: String::new(),
                align: None,
            }),
            Block::Image(Image {
                id: "i".into(),
                src: String::new(),
                alt: String::new(),
                href: None,
                width: Some(801),
            }),
            Block::Divider(Divider {
                id: "d".into(),
                color: Some("grey".into()),
            }),
            Block::Spacer(Spacer {
                id: "p".into(),
                height: 0,
            }),
        ]]);
        content.sections[0].padding = Some("2em".into());
        content.sections[0].background_color = Some("blue".into());
        let at: Vec<String> = check_shape(&content).into_iter().map(|i| i.at).collect();
        assert_eq!(
            at,
            [
                "/blocks/block/s1/background_color",
                "/blocks/block/s1/padding",
                "/blocks/block/h/level",
                "/blocks/block/i/width",
                "/blocks/block/d/color",
                "/blocks/block/p/height",
            ]
        );
    }

    #[test]
    fn the_font_list_resolves_every_name() {
        for (name, css) in FONT_STACKS {
            assert_eq!(font_css(name), Some(css));
        }
        assert_eq!(font_css("Arial"), None);
        assert_eq!(Align::Center.css(), "center");
        assert_eq!(heading("x").id(), "x");
    }
}
