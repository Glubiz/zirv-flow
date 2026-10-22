//! The block tree as MJML and as a text part (architecture 11.5, steps 5 and 6).
//!
//! One emitter walks the tree and asks a [`Fields`] source for the value of every text field.
//! The render path gives it fields that MiniJinja has already rendered; the conversion gives it
//! the sources themselves, merge tags kept. Because both come out of the same walk, the MJML of
//! the conversion rendered through MiniJinja and `mrml` is the document the block render sent
//! to `mrml`.

use std::convert::Infallible;

use super::model::{
    Align, Block, BlocksContent, Button, Heading, Image, RawHtml, Section, Settings, font_css,
};
use super::sanitize::{Cleaned, sanitize_source};
use super::tags::escape_literals;

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
}

/// The result of an emit: the MJML and the text part.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Emitted {
    pub(crate) mjml: String,
    pub(crate) text: String,
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
}

/// The heading font size for a level, in pixels.
const fn heading_size(level: u8) -> u32 {
    match level {
        1 => 28,
        2 => 22,
        _ => 18,
    }
}

/// Emit the MJML and the text part of `content`. The settings are applied here; their values
/// were checked by the caller (see `check_shape`), and an unknown font name falls back to the
/// first stack so the walk cannot fail on it.
pub(crate) fn emit<F: Fields>(
    content: &BlocksContent,
    fields: &mut F,
) -> Result<Emitted, F::Error> {
    let settings = &content.settings;
    let mut mjml = String::new();
    let mut text: Vec<String> = Vec::new();

    mjml.push_str("<mjml");
    if let Some(lang) = content.lang.as_deref().map(str::trim)
        && !lang.is_empty()
    {
        mjml.push_str(&format!(" lang=\"{lang}\""));
    }
    mjml.push_str(">\n<mj-head>\n<mj-attributes>\n");
    mjml.push_str(&format!(
        "<mj-all font-family=\"{}\" />\n",
        font_css(&settings.font_family).unwrap_or("Arial, Helvetica, sans-serif")
    ));
    mjml.push_str(&format!(
        "<mj-text color=\"{}\" line-height=\"1.5\" />\n",
        settings.text_color
    ));
    mjml.push_str("</mj-attributes>\n");
    mjml.push_str(&format!(
        "<mj-style>a{{color:{}}}</mj-style>\n",
        settings.link_color
    ));
    let preheader = fields.plain(&content.preheader)?;
    if !preheader.html.trim().is_empty() {
        mjml.push_str(&format!("<mj-preview>{}</mj-preview>\n", preheader.html));
    }
    mjml.push_str("</mj-head>\n");
    mjml.push_str(&format!(
        "<mj-body background-color=\"{}\" width=\"{}px\">\n",
        settings.background_color, settings.content_width
    ));
    for section in &content.sections {
        emit_section(section, settings, fields, &mut mjml, &mut text)?;
    }
    mjml.push_str("</mj-body>\n</mjml>\n");

    let mut text = text.join("\n\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Ok(Emitted { mjml, text })
}

fn emit_section<F: Fields>(
    section: &Section,
    settings: &Settings,
    fields: &mut F,
    mjml: &mut String,
    text: &mut Vec<String>,
) -> Result<(), F::Error> {
    mjml.push_str("<mj-section");
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
            emit_block(block, settings, fields, mjml, text)?;
        }
        mjml.push_str("</mj-column>\n");
    }
    mjml.push_str("</mj-section>\n");
    Ok(())
}

fn align_of(align: Option<Align>) -> &'static str {
    align.unwrap_or_default().css()
}

fn emit_block<F: Fields>(
    block: &Block,
    settings: &Settings,
    fields: &mut F,
    mjml: &mut String,
    text: &mut Vec<String>,
) -> Result<(), F::Error> {
    match block {
        Block::Heading(Heading {
            level,
            text: source,
            align,
            ..
        }) => {
            let value = fields.plain(source)?;
            let size = heading_size(*level);
            mjml.push_str(&format!(
                "<mj-text align=\"{}\" font-size=\"{size}px\" font-weight=\"bold\" line-height=\"1.3\"><h{level} style=\"margin:0;font-size:{size}px;font-weight:bold\">{}</h{level}></mj-text>\n",
                align_of(*align),
                value.html
            ));
            push_text(text, &value.text);
        }
        Block::Text(rich) => {
            let value = fields.rich(&rich.content)?;
            mjml.push_str(&format!(
                "<mj-text align=\"{}\">{}</mj-text>\n",
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
            ..
        }) => {
            let source = fields.url(src, UrlKind::ImageSource)?;
            let alt = fields.plain(alt)?;
            mjml.push_str(&format!(
                "<mj-image src=\"{}\" alt=\"{}\"",
                source.html, alt.html
            ));
            let link = match href {
                Some(href) => Some(fields.url(href, UrlKind::Link)?),
                None => None,
            };
            if let Some(link) = &link {
                mjml.push_str(&format!(" href=\"{}\"", link.html));
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
            label, url, align, ..
        }) => {
            let label = fields.plain(label)?;
            let url = fields.url(url, UrlKind::Link)?;
            mjml.push_str(&format!(
                "<mj-button href=\"{}\" align=\"{}\" background-color=\"{}\" color=\"{}\">{}</mj-button>\n",
                url.html,
                align.css(),
                settings.button_color,
                settings.button_text_color,
                label.html
            ));
            push_text(text, &format!("{}: {}", label.text.trim(), url.text));
        }
        Block::Divider(divider) => {
            mjml.push_str(&format!(
                "<mj-divider border-width=\"1px\" border-color=\"{}\" />\n",
                divider.color.as_deref().unwrap_or("#cccccc")
            ));
            push_text(text, "----------");
        }
        Block::Spacer(spacer) => {
            mjml.push_str(&format!("<mj-spacer height=\"{}px\" />\n", spacer.height));
        }
        Block::RawHtml(RawHtml { html, .. }) => {
            let value = fields.raw(html)?;
            mjml.push_str(&format!("<mj-raw>{value}</mj-raw>\n"));
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
    use super::*;

    fn content(blocks: Vec<Block>) -> BlocksContent {
        let mut content = BlocksContent::new("Hi");
        content.lang = Some("en".into());
        content.sections.push(Section {
            id: "s".into(),
            background_color: Some("#eeeeee".into()),
            padding: Some("10px 0".into()),
            columns: vec![blocks],
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
        let emitted = emit(&doc, &mut Sources).unwrap();
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
    fn every_block_has_its_element_and_its_text() {
        let doc = content(vec![
            Block::Heading(Heading {
                id: "h".into(),
                level: 2,
                text: "Big & bold".into(),
                align: Some(Align::Center),
            }),
            Block::Text(RichText {
                id: "t".into(),
                content: "<p>Body <a href=\"https://a.example\">link</a></p>".into(),
                align: None,
            }),
            Block::Image(Image {
                id: "i".into(),
                src: "https://a.example/i.png".into(),
                alt: "Logo".into(),
                href: Some("https://a.example".into()),
                width: Some(300),
            }),
            Block::Button(Button {
                id: "b".into(),
                label: "Shop".into(),
                url: "https://a.example/shop".into(),
                align: Align::Right,
            }),
            Block::Divider(Divider {
                id: "d".into(),
                color: None,
            }),
            Block::Spacer(Spacer {
                id: "p".into(),
                height: 12,
            }),
            Block::RawHtml(RawHtml {
                id: "r".into(),
                html: "<b>raw</b>".into(),
            }),
        ]);
        let emitted = emit(&doc, &mut Sources).unwrap();
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
                src: "https://a.example/i.png".into(),
                alt: alt.into(),
                href: href.map(str::to_owned),
                width: None,
            })
        };
        let text = |block| emit(&content(vec![block]), &mut Sources).unwrap().text;
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
            align: Align::Center,
        })]);
        let emitted = emit(&doc, &mut Sources).unwrap();
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
}
