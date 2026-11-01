//! The block tree as MJML and as a text part (architecture 11.5, steps 5 and 6).
//!
//! One emitter walks the tree and asks a [`Fields`] source for the value of every text field.
//! The render path gives it fields that MiniJinja has already rendered; the conversion gives it
//! the sources themselves, merge tags kept. Because both come out of the same walk, the MJML of
//! the conversion rendered through MiniJinja and `mrml` is the document the block render sent
//! to `mrml`.

use std::convert::Infallible;

use super::look::{Look, THEME_FOOTER_ID};
use super::model::{
    Align, Block, BlocksContent, Button, Heading, Image, ImageSrc, RawHtml, Section,
};
use super::sanitize::{Cleaned, sanitize_source};
use super::snippet::SnippetContent;
use super::tags::escape_literals;
use crate::content::mjml::UTM_OFF_MARK;

/// The value of a plain text field: safe to put in MJML, and as plain text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Plain {
    pub(crate) html: String,
    pub(crate) text: String,
}

/// What kind of link a URL field is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UrlKind {
    /// A link target: `https`, `http` or `mailto`.
    Link,
    /// An image source: `https`.
    ImageSource,
}

/// Where the emitter gets the value of each text field.
pub(crate) trait Fields {
    type Error;

    /// A plain text field, such as a heading, a label or an alt text.
    fn plain(&mut self, source: &str) -> Result<Plain, Self::Error>;
    /// A URL field.
    fn url(&mut self, source: &str, kind: UrlKind) -> Result<Plain, Self::Error>;
    /// A rich text field.
    fn rich(&mut self, source: &str) -> Result<Cleaned, Self::Error>;
    /// A raw HTML field.
    fn raw(&mut self, source: &str) -> Result<String, Self::Error>;
    /// The URL of an image: its `src` variant resolved.
    fn image_source(&mut self, src: &ImageSrc) -> Result<Plain, Self::Error>;
    /// Whether the section or block `id`, which holds a `visible_if`, is shown.
    fn visible(&mut self, id: &str) -> Result<bool, Self::Error>;
    /// The content of the snippet a snippet block names.
    fn snippet(&mut self, snippet_id: &str) -> Result<SnippetContent, Self::Error>;
    /// Whether each section and block gets the class `zf-block-<id>` (the marked preview).
    fn annotate(&self) -> bool {
        false
    }
}

/// The result of an emit: the MJML and the text part.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Emitted {
    pub(crate) mjml: String,
    pub(crate) text: String,
    /// The ids of the sections and blocks a false `visible_if` dropped, in document order.
    pub(crate) hidden: Vec<String>,
}

/// The fields as they are written: the literal text is escaped for MJML and the tags stay.
pub(crate) struct Sources;

impl Fields for Sources {
    type Error = Infallible;

    fn plain(&mut self, source: &str) -> Result<Plain, Infallible> {
        Ok(Plain {
            html: escape_literals(source),
            text: source.to_owned(),
        })
    }

    fn url(&mut self, source: &str, _kind: UrlKind) -> Result<Plain, Infallible> {
        self.plain(source)
    }

    fn rich(&mut self, source: &str) -> Result<Cleaned, Infallible> {
        Ok(sanitize_source(source))
    }

    fn raw(&mut self, source: &str) -> Result<String, Infallible> {
        Ok(source.to_owned())
    }

    /// Only a URL has a source to keep; the conversion refuses any other variant before it walks.
    fn image_source(&mut self, src: &ImageSrc) -> Result<Plain, Infallible> {
        match src {
            ImageSrc::Url(url) => self.plain(url),
            ImageSrc::Asset(_) | ImageSrc::Theme(_) => self.plain(""),
        }
    }

    /// The conversion refuses a document with a condition before it walks.
    fn visible(&mut self, _id: &str) -> Result<bool, Infallible> {
        Ok(true)
    }

    /// The conversion refuses a snippet block before it walks.
    fn snippet(&mut self, _snippet_id: &str) -> Result<SnippetContent, Infallible> {
        Ok(SnippetContent::default())
    }
}

/// `text` as an XML attribute value.
fn attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The heading font size for a level, in pixels.
const fn heading_size(level: u8) -> u32 {
    match level {
        1 => 28,
        2 => 22,
        _ => 18,
    }
}

/// Emit the MJML and the text part of `content`. The look (settings or theme) is applied here;
/// its values were checked by the caller (see `check_shape`), and an unknown font name falls
/// back to the first stack so the walk cannot fail on it. The theme footer, when the look has
/// one, closes the email as a section with the id `theme-footer`.
pub(crate) fn emit<F: Fields>(
    content: &BlocksContent,
    look: &Look<'_>,
    fields: &mut F,
) -> Result<Emitted, F::Error> {
    let mut mjml = String::new();
    let mut text: Vec<String> = Vec::new();
    let mut hidden: Vec<String> = Vec::new();

    mjml.push_str("<mjml");
    if let Some(lang) = content.lang.as_deref().map(str::trim)
        && !lang.is_empty()
    {
        mjml.push_str(&format!(" lang=\"{lang}\""));
    }
    mjml.push_str(">\n<mj-head>\n");
    if let Some(url) = look.web_font_url {
        mjml.push_str(&format!(
            "<mj-font name=\"{}\" href=\"{}\" />\n",
            attribute(look.font_name()),
            attribute(url)
        ));
    }
    mjml.push_str("<mj-attributes>\n");
    mjml.push_str(&format!("<mj-all font-family=\"{}\" />\n", look.font_stack));
    mjml.push_str(&format!(
        "<mj-text color=\"{}\" line-height=\"1.5\" />\n",
        look.text
    ));
    mjml.push_str("</mj-attributes>\n");
    let decoration = match look.link_underline {
        Some(true) => ";text-decoration:underline",
        Some(false) => ";text-decoration:none",
        None => "",
    };
    mjml.push_str(&format!(
        "<mj-style>a{{color:{}{decoration}}}</mj-style>\n",
        look.link
    ));
    let preheader = fields.plain(&content.preheader)?;
    if !preheader.html.trim().is_empty() {
        mjml.push_str(&format!("<mj-preview>{}</mj-preview>\n", preheader.html));
    }
    mjml.push_str("</mj-head>\n");
    mjml.push_str(&format!(
        "<mj-body background-color=\"{}\" width=\"{}px\">\n",
        look.background, look.content_width
    ));
    for section in &content.sections {
        if section.visible_if.is_some() && !fields.visible(&section.id)? {
            hidden.push(section.id.clone());
            continue;
        }
        emit_section(section, look, fields, &mut mjml, &mut text, &mut hidden)?;
    }
    if let Some(footer) = look.footer {
        let section = Section {
            id: THEME_FOOTER_ID.to_owned(),
            background_color: None,
            padding: None,
            columns: vec![footer.blocks.clone()],
            visible_if: None,
        };
        emit_section(&section, look, fields, &mut mjml, &mut text, &mut hidden)?;
    }
    mjml.push_str("</mj-body>\n</mjml>\n");

    let mut text = text.join("\n\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Ok(Emitted { mjml, text, hidden })
}

fn emit_section<F: Fields>(
    section: &Section,
    look: &Look<'_>,
    fields: &mut F,
    mjml: &mut String,
    text: &mut Vec<String>,
    hidden: &mut Vec<String>,
) -> Result<(), F::Error> {
    mjml.push_str("<mj-section");
    mjml.push_str(&css_class(fields, &section.id));
    if let Some(color) = &section.background_color {
        mjml.push_str(&format!(" background-color=\"{color}\""));
    }
    if let Some(padding) = &section.padding {
        mjml.push_str(&format!(" padding=\"{padding}\""));
    }
    mjml.push_str(">\n");
    for column in &section.columns {
        mjml.push_str("<mj-column>\n");
        for block in column {
            if block.visible_if().is_some() && !fields.visible(block.id())? {
                hidden.push(block.id().to_owned());
                continue;
            }
            emit_block(block, block.id(), look, fields, mjml, text)?;
        }
        mjml.push_str("</mj-column>\n");
    }
    mjml.push_str("</mj-section>\n");
    Ok(())
}

fn align_of(align: Option<Align>) -> &'static str {
    align.unwrap_or_default().css()
}

/// The attribute that gives an element the class `zf-block-<id>`, only for a marked preview.
/// An id that is not `[A-Za-z0-9_-]` gets none, so it can never break out of the attribute.
fn css_class<F: Fields>(fields: &F, id: &str) -> String {
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if fields.annotate() && safe {
        format!(" css-class=\"zf-block-{id}\"")
    } else {
        String::new()
    }
}

/// Emit one block. `class_id` names the class it carries: the blocks of a snippet carry the id of
/// the snippet block.
fn emit_block<F: Fields>(
    block: &Block,
    class_id: &str,
    look: &Look<'_>,
    fields: &mut F,
    mjml: &mut String,
    text: &mut Vec<String>,
) -> Result<(), F::Error> {
    let class = css_class(fields, class_id);
    match block {
        Block::Heading(Heading {
            level,
            text: source,
            align,
            ..
        }) => {
            let value = fields.plain(source)?;
            let size = heading_size(*level);
            let color = look
                .heading
                .map(|color| format!(" color=\"{color}\""))
                .unwrap_or_default();
            mjml.push_str(&format!(
                "<mj-text{class}{color} align=\"{}\" font-size=\"{size}px\" font-weight=\"bold\" line-height=\"1.3\"><h{level} style=\"margin:0;font-size:{size}px;font-weight:bold\">{}</h{level}></mj-text>\n",
                align_of(*align),
                value.html
            ));
            push_text(text, &value.text);
        }
        Block::Text(rich) => {
            let value = fields.rich(&rich.content)?;
            mjml.push_str(&format!(
                "<mj-text{class} align=\"{}\">{}</mj-text>\n",
                align_of(rich.align),
                value.html
            ));
            push_text(text, &value.text);
        }
        Block::Image(Image {
            src,
            alt,
            href,
            width,
            utm_off,
            ..
        }) => {
            let source = fields.image_source(src)?;
            let alt = fields.plain(alt)?;
            mjml.push_str(&format!(
                "<mj-image{class} src=\"{}\" alt=\"{}\"",
                source.html, alt.html
            ));
            let link = href
                .as_deref()
                .map(|href| fields.url(href, UrlKind::Link))
                .transpose()?;
            if let Some(link) = &link {
                mjml.push_str(&format!(" href=\"{}\"", link.html));
                if *utm_off {
                    mjml.push_str(UTM_OFF_MARK);
                }
            }
            if let Some(width) = width {
                mjml.push_str(&format!(" width=\"{width}px\""));
            }
            mjml.push_str(" />\n");
            match (alt.text.trim(), &link) {
                ("", Some(link)) => push_text(text, &link.text),
                ("", None) => {}
                (alt, Some(link)) => push_text(text, &format!("{alt}: {}", link.text)),
                (alt, None) => push_text(text, alt),
            }
        }
        Block::Button(Button {
            label,
            url,
            align,
            utm_off,
            ..
        }) => {
            let label = fields.plain(label)?;
            let url = fields.url(url, UrlKind::Link)?;
            let mark = if *utm_off { UTM_OFF_MARK } else { "" };
            let radius = look
                .button_radius
                .map(|radius| format!(" border-radius=\"{radius}px\""))
                .unwrap_or_default();
            mjml.push_str(&format!(
                "<mj-button{class} href=\"{}\"{mark} align=\"{}\" background-color=\"{}\" color=\"{}\"{radius}>{}</mj-button>\n",
                url.html,
                align.css(),
                look.button,
                look.button_text,
                label.html
            ));
            push_text(text, &format!("{}: {}", label.text.trim(), url.text));
        }
        // A snippet holds no snippet block (the checks refuse one); a stray one is skipped, so
        // the walk cannot loop.
        Block::Snippet(snippet) => {
            let content = fields.snippet(&snippet.snippet_id)?;
            for inner in &content.blocks {
                if !matches!(inner, Block::Snippet(_)) {
                    emit_block(inner, class_id, look, fields, mjml, text)?;
                }
            }
        }
        Block::Divider(divider) => {
            mjml.push_str(&format!(
                "<mj-divider{class} border-width=\"1px\" border-color=\"{}\" />\n",
                divider.color.as_deref().unwrap_or("#cccccc")
            ));
            push_text(text, "----------");
        }
        Block::Spacer(spacer) => {
            mjml.push_str(&format!(
                "<mj-spacer{class} height=\"{}px\" />\n",
                spacer.height
            ));
        }
        Block::RawHtml(RawHtml { html, .. }) => {
            let value = fields.raw(html)?;
            if class.is_empty() {
                mjml.push_str(&format!("<mj-raw>{value}</mj-raw>\n"));
            } else {
                // `mj-raw` takes no `css-class`, so a marked preview wraps it.
                mjml.push_str(&format!(
                    "<mj-raw><div class=\"zf-block-{class_id}\">{value}</div></mj-raw>\n"
                ));
            }
        }
    }
    Ok(())
}

fn push_text(parts: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        parts.push(text.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Divider, RichText, Spacer};
    use super::super::snippet::SnippetContent;
    use super::*;

    fn content(blocks: Vec<Block>) -> BlocksContent {
        let mut content = BlocksContent::new("Hi");
        content.lang = Some("en".into());
        content.sections.push(Section {
            id: "s".into(),
            background_color: Some("#eeeeee".into()),
            padding: Some("10px 0".into()),
            columns: vec![blocks],
            visible_if: None,
        });
        content
    }

    #[test]
    fn the_document_frame_carries_the_settings() {
        let mut doc = content(Vec::new());
        doc.settings.font_family = "georgia".into();
        doc.settings.content_width = 640;
        doc.settings.background_color = "#f0f0f0".into();
        doc.preheader = "Preview & more".into();
        let emitted = emit(&doc, &Look::of_settings(&doc.settings), &mut Sources).unwrap();
        for part in [
            "<mjml lang=\"en\">",
            "<mj-all font-family=\"Georgia, 'Times New Roman', serif\" />",
            "<mj-text color=\"#222222\" line-height=\"1.5\" />",
            "<mj-style>a{color:#1a5fb4}</mj-style>",
            "<mj-preview>Preview &amp; more</mj-preview>",
            "<mj-body background-color=\"#f0f0f0\" width=\"640px\">",
            "<mj-section background-color=\"#eeeeee\" padding=\"10px 0\">",
        ] {
            assert!(emitted.mjml.contains(part), "{part}\n{}", emitted.mjml);
        }
        assert_eq!(emitted.text, "");
    }

    #[test]
    fn a_button_and_a_linked_image_carry_the_utm_opt_out_and_the_rest_do_not() {
        let mut doc = content(vec![
            Block::Button(Button {
                id: "b".into(),
                label: "Go".into(),
                url: "https://a.example/b".into(),
                utm_off: true,
                align: Align::Left,
                visible_if: None,
            }),
            Block::Image(Image {
                id: "i".into(),
                src: ImageSrc::Url("https://a.example/i.png".into()),
                alt: "Logo".into(),
                href: Some("https://a.example/i".into()),
                utm_off: true,
                width: None,
                visible_if: None,
            }),
        ]);
        let mark = crate::content::mjml::UTM_OFF_MARK;
        let emitted = emit(&doc, &Look::of_settings(&doc.settings), &mut Sources).unwrap();
        assert_eq!(emitted.mjml.matches(mark).count(), 2, "{}", emitted.mjml);
        let compiled = crate::content::mjml::compile(&emitted.mjml).unwrap();
        assert_eq!(compiled.html.matches(" data-zf-utm=\"off\"").count(), 2);
        for section in &mut doc.sections {
            for block in section.columns.iter_mut().flatten() {
                match block {
                    Block::Button(button) => button.utm_off = false,
                    Block::Image(image) => image.utm_off = false,
                    _ => {}
                }
            }
        }
        let emitted = emit(&doc, &Look::of_settings(&doc.settings), &mut Sources).unwrap();
        assert!(!emitted.mjml.contains(mark));
    }

    #[test]
    fn every_block_has_its_element_and_its_text() {
        let doc = content(vec![
            Block::Heading(Heading {
                id: "h".into(),
                level: 2,
                text: "Big & bold".into(),
                align: Some(Align::Center),
                visible_if: None,
            }),
            Block::Text(RichText {
                id: "t".into(),
                content: "<p>Body <a href=\"https://a.example\">link</a></p>".into(),
                align: None,
                visible_if: None,
            }),
            Block::Image(Image {
                id: "i".into(),
                src: ImageSrc::Url("https://a.example/i.png".into()),
                alt: "Logo".into(),
                href: Some("https://a.example".into()),
                utm_off: false,
                width: Some(300),
                visible_if: None,
            }),
            Block::Button(Button {
                id: "b".into(),
                label: "Shop".into(),
                url: "https://a.example/shop".into(),
                utm_off: false,
                align: Align::Right,
                visible_if: None,
            }),
            Block::Divider(Divider {
                id: "d".into(),
                color: None,
                visible_if: None,
            }),
            Block::Spacer(Spacer {
                id: "p".into(),
                height: 12,
                visible_if: None,
            }),
            Block::RawHtml(RawHtml {
                id: "r".into(),
                html: "<b>raw</b>".into(),
                visible_if: None,
            }),
        ]);
        let emitted = emit(&doc, &Look::of_settings(&doc.settings), &mut Sources).unwrap();
        for part in [
            "<mj-text align=\"center\" font-size=\"22px\" font-weight=\"bold\" line-height=\"1.3\"><h2 style=\"margin:0;font-size:22px;font-weight:bold\">Big &amp; bold</h2></mj-text>",
            "<mj-text align=\"left\"><p>Body <a href=\"https://a.example\">link</a></p></mj-text>",
            "<mj-image src=\"https://a.example/i.png\" alt=\"Logo\" href=\"https://a.example\" width=\"300px\" />",
            "<mj-button href=\"https://a.example/shop\" align=\"right\" background-color=\"#1a5fb4\" color=\"#ffffff\">Shop</mj-button>",
            "<mj-divider border-width=\"1px\" border-color=\"#cccccc\" />",
            "<mj-spacer height=\"12px\" />",
            "<mj-raw><b>raw</b></mj-raw>",
        ] {
            assert!(emitted.mjml.contains(part), "{part}\n{}", emitted.mjml);
        }
        assert_eq!(
            emitted.text,
            "Big & bold\n\nBody link: https://a.example\n\nLogo: https://a.example\n\nShop: https://a.example/shop\n\n----------\n"
        );
    }

    #[test]
    fn an_image_without_alt_or_link_has_no_text_and_a_bare_link_is_the_url() {
        let image = |alt: &str, href: Option<&str>| {
            Block::Image(Image {
                id: "i".into(),
                src: ImageSrc::Url("https://a.example/i.png".into()),
                alt: alt.into(),
                utm_off: false,
                href: href.map(str::to_owned),
                width: None,
                visible_if: None,
            })
        };
        let text = |block| {
            let doc = content(vec![block]);
            emit(&doc, &Look::of_settings(&doc.settings), &mut Sources)
                .unwrap()
                .text
        };
        assert_eq!(text(image("", None)), "");
        assert_eq!(text(image("Alt", None)), "Alt\n");
        assert_eq!(
            text(image("", Some("https://b.example"))),
            "https://b.example\n"
        );
    }

    #[test]
    fn merge_tags_stay_in_the_emitted_sources_and_only_the_text_around_them_is_escaped() {
        let doc = content(vec![Block::Button(Button {
            id: "b".into(),
            label: "Go, {{ person.name | default(\"you\") }} & co".into(),
            url: "{{ system.unsubscribe_url }}".into(),
            utm_off: false,
            align: Align::Center,
            visible_if: None,
        })]);
        let emitted = emit(&doc, &Look::of_settings(&doc.settings), &mut Sources).unwrap();
        assert!(
            emitted
                .mjml
                .contains(">Go, {{ person.name | default(\"you\") }} &amp; co</mj-button>"),
            "{}",
            emitted.mjml
        );
        assert!(
            emitted
                .mjml
                .contains("href=\"{{ system.unsubscribe_url }}\"")
        );
        assert_eq!(
            emitted.text,
            "Go, {{ person.name | default(\"you\") }} & co: {{ system.unsubscribe_url }}\n"
        );
    }

    #[test]
    fn a_theme_look_adds_the_font_the_radius_the_heading_colour_and_the_footer() {
        let theme = crate::theme::ThemeTokens {
            web_font_url: Some("https://fonts.example/f.css?a=1&b=2".into()),
            link_underline: false,
            button_radius: 9,
            ..Default::default()
        };
        let footer = SnippetContent {
            blocks: vec![Block::Spacer(Spacer {
                id: "gap".into(),
                height: 5,
                visible_if: None,
            })],
            sections: Vec::new(),
        };
        let mut doc = content(vec![
            Block::Heading(Heading {
                id: "h".into(),
                level: 1,
                text: "Hi".into(),
                align: None,
                visible_if: None,
            }),
            Block::Button(Button {
                id: "b".into(),
                label: "Go".into(),
                url: "https://a.example".into(),
                utm_off: false,
                align: Align::Left,
                visible_if: None,
            }),
        ]);
        doc.settings.font_family = "arial".into();
        let mut look = Look::of_settings(&doc.settings);
        look.heading = Some(&theme.primary);
        look.link_underline = Some(theme.link_underline);
        look.button_radius = Some(theme.button_radius);
        look.web_font_url = theme.web_font_url.as_deref();
        look.footer = Some(&footer);
        let mjml = emit(&doc, &look, &mut Sources).unwrap().mjml;
        for part in [
            "<mj-font name=\"Arial\" href=\"https://fonts.example/f.css?a=1&amp;b=2\" />",
            "<mj-style>a{color:#1a5fb4;text-decoration:none}</mj-style>",
            "<mj-text color=\"#1a5fb4\" align=",
            "border-radius=\"9px\">Go</mj-button>",
            "<mj-spacer height=\"5px\" />\n</mj-column>\n</mj-section>\n</mj-body>",
        ] {
            assert!(mjml.contains(part), "{part}\n{mjml}");
        }
    }

    #[test]
    fn a_snippet_block_emits_the_blocks_the_fields_source_gives_for_it() {
        struct One;
        impl Fields for One {
            type Error = Infallible;
            fn plain(&mut self, source: &str) -> Result<Plain, Infallible> {
                Sources.plain(source)
            }
            fn url(&mut self, source: &str, kind: UrlKind) -> Result<Plain, Infallible> {
                Sources.url(source, kind)
            }
            fn rich(&mut self, source: &str) -> Result<Cleaned, Infallible> {
                Sources.rich(source)
            }
            fn raw(&mut self, source: &str) -> Result<String, Infallible> {
                Sources.raw(source)
            }
            fn image_source(&mut self, src: &ImageSrc) -> Result<Plain, Infallible> {
                Sources.image_source(src)
            }
            fn visible(&mut self, _id: &str) -> Result<bool, Infallible> {
                Ok(true)
            }
            fn snippet(&mut self, _snippet_id: &str) -> Result<SnippetContent, Infallible> {
                Ok(SnippetContent {
                    blocks: vec![
                        Block::Spacer(Spacer {
                            id: "gap".into(),
                            height: 7,
                            visible_if: None,
                        }),
                        Block::Snippet(super::super::model::Snippet {
                            id: "again".into(),
                            snippet_id: "x".into(),
                            visible_if: None,
                        }),
                    ],
                    sections: Vec::new(),
                })
            }
        }
        let doc = content(vec![Block::Snippet(super::super::model::Snippet {
            id: "sn".into(),
            snippet_id: "x".into(),
            visible_if: None,
        })]);
        let mjml = emit(&doc, &Look::of_settings(&doc.settings), &mut One)
            .unwrap()
            .mjml;
        assert_eq!(
            mjml.matches("<mj-spacer height=\"7px\" />").count(),
            1,
            "{mjml}"
        );
    }

    /// The conversion's fields with the marked preview switched on.
    struct Marked;

    impl Fields for Marked {
        type Error = Infallible;
        fn plain(&mut self, source: &str) -> Result<Plain, Infallible> {
            Sources.plain(source)
        }
        fn url(&mut self, source: &str, kind: UrlKind) -> Result<Plain, Infallible> {
            Sources.url(source, kind)
        }
        fn rich(&mut self, source: &str) -> Result<Cleaned, Infallible> {
            Sources.rich(source)
        }
        fn raw(&mut self, source: &str) -> Result<String, Infallible> {
            Sources.raw(source)
        }
        fn image_source(&mut self, src: &ImageSrc) -> Result<Plain, Infallible> {
            Sources.image_source(src)
        }
        fn visible(&mut self, id: &str) -> Result<bool, Infallible> {
            Sources.visible(id)
        }
        fn snippet(&mut self, id: &str) -> Result<SnippetContent, Infallible> {
            Sources.snippet(id)
        }
        fn annotate(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_marked_emit_classes_every_element_and_an_unsafe_id_gets_no_class() {
        let spacer = |id: &str| {
            Block::Spacer(Spacer {
                id: id.into(),
                height: 10,
                visible_if: None,
            })
        };
        let raw = Block::RawHtml(RawHtml {
            id: "r".into(),
            html: "<b>x</b>".into(),
            visible_if: None,
        });
        let doc = content(vec![spacer("a-1"), raw, spacer("x\" onload=\"{")]);
        let look = Look::of_settings(&doc.settings);

        let marked = emit(&doc, &look, &mut Marked).unwrap().mjml;
        let plain = emit(&doc, &look, &mut Sources).unwrap().mjml;

        assert!(
            marked.contains("<mj-section css-class=\"zf-block-s\""),
            "{marked}"
        );
        assert!(
            marked.contains("<mj-spacer css-class=\"zf-block-a-1\""),
            "{marked}"
        );
        assert!(
            marked.contains("<div class=\"zf-block-r\"><b>x</b></div>"),
            "{marked}"
        );
        assert_eq!(marked.matches("css-class").count(), 2);
        assert!(!marked.contains("onload"));
        assert!(!plain.contains("zf-block-"), "{plain}");
    }
}
