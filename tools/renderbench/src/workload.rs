//! The message the benchmark renders: a typical block email, built in memory (no database) with
//! the render inputs a send resolves, in the shape a send gives them (architecture 11.4).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use zirv_flow_templates::{
    Block, FieldCatalog, Loaded, RenderContext, RenderExtras, TemplateDocument, render_extras,
};

/// The base of the document: the `newsletter` fixture of the block renderer's golden tests.
const NEWSLETTER: &str =
    include_str!("../../../crates/zirv-flow-templates/tests/fixtures/blocks/newsletter.doc.json");

const SNIPPET_PROMO: &str = "0192b1a4-7c3e-7000-8000-00000000c001";
const SNIPPET_FOOTER: &str = "0192b1a4-7c3e-7000-8000-00000000c002";
const ASSET_PHOTO: &str = "0192b1a4-7c3e-7000-8000-00000000a101";
const ASSET_LOGO: &str = "0192b1a4-7c3e-7000-8000-00000000a102";
const THEME_NAME: &str = "Spring brand";

/// The document, the context of one person and the resolved inputs of one render.
pub(crate) struct Workload {
    pub(crate) doc: TemplateDocument,
    pub(crate) context: RenderContext,
    pub(crate) extras: RenderExtras,
}

/// The sections added to the newsletter to reach about 20 blocks: an image from an asset, a
/// snippet, a section and two blocks with a `visible_if`, and one of each remaining kind.
fn added_sections() -> Vec<Value> {
    vec![
        json!({
            "id": "photo",
            "columns": [[
                {"type": "image", "id": "photo_image", "src": {"asset": ASSET_PHOTO},
                 "alt": "A pair of shoes for {{ person.name }}", "href": "https://shop.example/shoes"},
                {"type": "snippet", "id": "promo", "snippet_id": SNIPPET_PROMO}
            ]]
        }),
        json!({
            "id": "plan_offer",
            "visible_if": {"trait": "plan", "eq": "pro"},
            "columns": [[
                {"type": "heading", "id": "offer_title", "level": 2, "text": "For {{ person.plan }} members"},
                {"type": "text", "id": "offer_text",
                 "content": "<p>Your <b>{{ person.plan }}</b> plan includes free returns on every order.</p>"}
            ]]
        }),
        json!({
            "id": "extras",
            "columns": [[
                {"type": "button", "id": "vip_button", "label": "Open the VIP room",
                 "url": "https://shop.example/vip", "visible_if": {"trait": "plan", "eq": "vip"}},
                {"type": "spacer", "id": "gap", "height": 16},
                {"type": "raw_html", "id": "badge", "html": "<p style=\"margin:0\">Free shipping over 50.</p>"},
                {"type": "text", "id": "tip", "visible_if": {"trait": "name", "present": true},
                 "content": "<p>Tip for {{ person.name }}: <a href=\"https://shop.example/care\">care guide</a>.</p>"},
                {"type": "divider", "id": "end_rule"},
                {"type": "image", "id": "banner", "src": {"url": "https://img.example/banner.png"},
                 "alt": "Banner"}
            ]]
        }),
    ]
}

fn document() -> Result<TemplateDocument, String> {
    let mut value: Value = serde_json::from_str(NEWSLETTER).map_err(|e| e.to_string())?;
    value["blocks"]["category"] = json!("marketing");
    value["blocks"]["theme"] = json!("default");
    value["blocks"]["footer"] = json!("theme");
    let sections = value["blocks"]["sections"]
        .as_array_mut()
        .ok_or("the newsletter has no sections")?;
    sections.extend(added_sections());
    zirv_flow_templates::parse_template_document(&value).map_err(|e| format!("{e:?}"))
}

/// What a send loads for this document, as the opaque JSON the store keeps.
fn loaded_values() -> (Value, Value, Value) {
    let theme = json!({
        "primary": "#0b5fff", "text": "#1f2933", "background": "#ffffff", "link": "#0b5fff",
        "button": "#0b5fff", "button_text": "#ffffff", "content_width": 600, "font": "georgia",
        "button_radius": 6, "link_underline": true,
        "logo": ASSET_LOGO, "footer_snippet": SNIPPET_FOOTER
    });
    let promo = json!({"blocks": [
        {"type": "heading", "id": "promo_title", "level": 2, "text": "Spring sale for {{ person.name }}"},
        {"type": "button", "id": "promo_button", "label": "See the sale", "url": "https://shop.example/sale"}
    ]});
    let footer = json!({"blocks": [
        {"type": "image", "id": "footer_logo", "src": {"theme": "logo"}, "alt": "Spring brand"},
        {"type": "text", "id": "legal",
         "content": "<p>You get this because you signed up. <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a> or <a href=\"{{ system.preferences_url }}\">change your preferences</a>.</p>"}
    ]});
    (theme, promo, footer)
}

/// The traits the layout uses, plus the reserved address.
pub(crate) fn catalog() -> Result<FieldCatalog, String> {
    let catalog: FieldCatalog = serde_json::from_value(json!({
        "person": {"name": "string", "plan": "string"},
        "event": {}
    }))
    .map_err(|e| e.to_string())?;
    Ok(catalog.with_reserved_email())
}

/// The person every render is for. Values are synthetic.
fn person(catalog: &FieldCatalog) -> RenderContext {
    let traits = json!({"name": "Ann O'Neil", "plan": "pro"});
    let mut context =
        RenderContext::real_person(catalog, &traits, Some("ann@example.invalid".into()));
    context.message.id = "msg_bench".to_owned();
    context.system.unsubscribe_url = "https://flow.example/t/u/bench".to_owned();
    context.system.preferences_url = "https://flow.example/t/p/bench".to_owned();
    context
}

impl Workload {
    pub(crate) fn build() -> Result<Self, String> {
        let doc = document()?;
        let (theme, promo, footer) = loaded_values();
        let assets = BTreeMap::from([
            (
                ASSET_PHOTO.to_owned(),
                "https://assets.example/shoes.jpg".to_owned(),
            ),
            (
                ASSET_LOGO.to_owned(),
                "https://assets.example/logo.png".to_owned(),
            ),
        ]);
        // Two of the three conditions hold for this person, so one block is hidden.
        let conditions = BTreeMap::from([
            ("plan_offer".to_owned(), true),
            ("vip_button".to_owned(), false),
            ("tip".to_owned(), true),
        ]);
        let extras = render_extras(Loaded {
            theme: Some((THEME_NAME, &theme)),
            snippets: vec![(SNIPPET_PROMO, &promo), (SNIPPET_FOOTER, &footer)],
            assets,
            conditions,
        });
        Ok(Self {
            doc,
            context: person(&catalog()?),
            extras,
        })
    }

    /// How many blocks the document has, not counting those a snippet adds.
    pub(crate) fn block_count(&self) -> usize {
        self.doc.blocks.as_ref().map_or(0, |content| {
            content
                .sections
                .iter()
                .flat_map(|section| section.columns.iter().flatten())
                .count()
        })
    }

    pub(crate) fn snippet_blocks(&self) -> usize {
        self.doc.blocks.as_ref().map_or(0, |content| {
            content
                .sections
                .iter()
                .flat_map(|section| section.columns.iter().flatten())
                .filter(|block| matches!(block, Block::Snippet(_)))
                .count()
        })
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_templates::{Hooks, ThemeRef, render_email_with, validate_with};

    use super::*;

    fn built() -> Workload {
        Workload::build().expect("the workload builds")
    }

    #[test]
    fn the_document_has_twenty_blocks_a_theme_and_one_snippet() {
        let workload = built();
        assert_eq!(workload.block_count(), 20);
        assert_eq!(workload.snippet_blocks(), 1);
        let content = workload.doc.blocks.as_ref().expect("block content");
        assert_eq!(content.theme, ThemeRef::Default);
        assert_eq!(workload.extras.theme_name.as_deref(), Some(THEME_NAME));
        let theme = workload.extras.theme.as_ref().expect("a theme");
        assert_eq!(theme.logo.as_deref(), Some(ASSET_LOGO));
        assert_eq!(theme.footer_snippet.as_deref(), Some(SNIPPET_FOOTER));
        assert_eq!(workload.extras.snippets.len(), 2);
        assert!(workload.extras.theme_logo_url().is_some());
        assert!(workload.extras.asset_url(ASSET_PHOTO).is_some());
    }

    #[test]
    fn three_conditions_are_passed_in_already_evaluated() {
        let workload = built();
        let content = workload.doc.blocks.as_ref().expect("block content");
        let ids: Vec<&str> = zirv_flow_templates::conditions(content)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["plan_offer", "vip_button", "tip"]);
        for id in ids {
            assert!(workload.extras.conditions.contains_key(id), "{id}");
        }
    }

    #[test]
    fn the_document_validates_with_no_blocking_issue() {
        let workload = built();
        let issues = validate_with(
            &workload.doc,
            &catalog().expect("catalog"),
            &workload.extras,
        );
        let blocking: Vec<_> = issues.iter().filter(|issue| issue.is_blocking()).collect();
        assert!(blocking.is_empty(), "{blocking:?}");
    }

    #[test]
    fn the_message_renders_with_the_theme_footer_the_snippet_the_asset_and_one_hidden_block() {
        let workload = built();
        let rendered = render_email_with(
            &workload.doc,
            &workload.context,
            &Hooks::untracked(),
            &workload.extras,
        )
        .expect("the message renders");
        assert!(rendered.html.contains("https://assets.example/shoes.jpg"));
        assert!(rendered.html.contains("https://assets.example/logo.png"));
        assert!(rendered.html.contains("Spring sale for"));
        assert!(rendered.html.contains("change your preferences"));
        assert!(rendered.html.contains("pro"));
        let hidden: Vec<&str> = rendered.hidden.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(hidden, ["vip_button"]);
    }
}
