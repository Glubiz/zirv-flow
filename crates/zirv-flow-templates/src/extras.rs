//! The two pure halves of resolving a block email's render inputs (architecture 11.4): what a
//! document asks for, and how what the caller loaded becomes [`RenderExtras`]. The loading itself
//! (the theme, the snippets, the asset rows and the condition results) is I/O, so the callers do
//! it; `api` and `jobs` both go through these two functions, so a preview and a send build the
//! same extras from the same loaded values.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::content::blocks::{
    Block, BlocksContent, ImageSrc, SnippetContent, ThemeRef, conditions,
};
use crate::context::RenderExtras;
use crate::document::{Mode, TemplateDocument};
use crate::theme::ThemeTokens;

/// Which theme a document asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeNeed {
    /// No theme: `theme: "none"`, or a document that is not a block email.
    None,
    /// The workspace default theme, if it has one.
    Default,
    /// The theme with this id, as the document spells it.
    Id(String),
}

/// What a document names that the caller must resolve before it can render or validate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtrasNeeds {
    pub theme: ThemeNeed,
    /// The ids of the snippets its snippet blocks name, as spelled, each once.
    pub snippets: Vec<String>,
    /// The ids of the assets its own images name, as spelled, each once. The assets of snippets
    /// and of the theme are found once those are loaded.
    pub assets: Vec<String>,
    /// Each `visible_if` with the id of the section or block that holds it.
    pub conditions: Vec<(String, Value)>,
}

impl ExtrasNeeds {
    fn nothing() -> Self {
        Self {
            theme: ThemeNeed::None,
            snippets: Vec::new(),
            assets: Vec::new(),
            conditions: Vec::new(),
        }
    }
}

/// What `doc` asks for. A document that is not a block email asks for nothing.
pub fn extras_needs(doc: &TemplateDocument) -> ExtrasNeeds {
    match doc.blocks.as_ref().filter(|_| doc.mode == Mode::Blocks) {
        Some(content) => block_needs(content),
        None => ExtrasNeeds::nothing(),
    }
}

fn block_needs(content: &BlocksContent) -> ExtrasNeeds {
    let mut needs = ExtrasNeeds {
        theme: match &content.theme {
            ThemeRef::None => ThemeNeed::None,
            ThemeRef::Default => ThemeNeed::Default,
            ThemeRef::Id(id) => ThemeNeed::Id(id.clone()),
        },
        conditions: conditions(content)
            .into_iter()
            .map(|(id, condition)| (id.to_owned(), condition.clone()))
            .collect(),
        ..ExtrasNeeds::nothing()
    };
    let blocks = content
        .sections
        .iter()
        .flat_map(|section| section.columns.iter().flatten());
    for block in blocks {
        match block {
            Block::Snippet(reference) if !needs.snippets.contains(&reference.snippet_id) => {
                needs.snippets.push(reference.snippet_id.clone());
            }
            Block::Image(image) => {
                if let ImageSrc::Asset(id) = &image.src
                    && !needs.assets.contains(id)
                {
                    needs.assets.push(id.clone());
                }
            }
            _ => {}
        }
    }
    needs
}

/// What the caller loaded, as the opaque JSON the store keeps.
#[derive(Clone, Debug, Default)]
pub struct Loaded<'a> {
    /// The theme in effect: its name and its tokens. `None` when the document has no theme or
    /// the one it names could not be resolved.
    pub theme: Option<(&'a str, &'a Value)>,
    /// The content of each snippet that resolved, by the id as the document spells it.
    pub snippets: Vec<(&'a str, &'a Value)>,
    /// The `https` URL of each asset that resolved, by the id as the document spells it.
    pub assets: BTreeMap<String, String>,
    /// The result of each `visible_if`, by the id of its section or block.
    pub conditions: BTreeMap<String, bool>,
}

/// The extras a render or validation takes. A theme or a snippet that does not parse is left
/// out, exactly as one that was not found: the renderer reports it as a broken reference.
pub fn render_extras(loaded: Loaded<'_>) -> RenderExtras {
    let mut extras = RenderExtras {
        assets: loaded.assets,
        conditions: loaded.conditions,
        ..RenderExtras::default()
    };
    if let Some((name, tokens)) = loaded.theme
        && let Ok(tokens) = serde_json::from_value::<ThemeTokens>(tokens.clone())
    {
        extras.theme = Some(tokens);
        extras.theme_name = Some(name.to_owned());
    }
    for (id, content) in loaded.snippets {
        if let Ok(content) = serde_json::from_value::<SnippetContent>(content.clone()) {
            extras.snippets.insert(id.to_owned(), content);
        }
    }
    extras
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::content::blocks::parse_template_document;

    const SNIPPET: &str = "0192b1a4-7c3e-7000-8000-00000000c001";
    const ASSET: &str = "0192b1a4-7c3e-7000-8000-00000000a001";

    fn document(theme: Value) -> TemplateDocument {
        parse_template_document(&json!({
            "schema": 2, "channel": "email", "mode": "blocks",
            "blocks": {
                "subject": "Hi", "category": "transactional", "theme": theme,
                "sections": [{"id": "s1", "visible_if": {"list": SNIPPET}, "columns": [[
                    {"type": "snippet", "id": "n1", "snippet_id": SNIPPET},
                    {"type": "snippet", "id": "n2", "snippet_id": SNIPPET},
                    {"type": "image", "id": "i1", "alt": "x", "src": {"asset": ASSET}},
                    {"type": "image", "id": "i2", "alt": "x", "src": {"asset": ASSET}},
                    {"type": "image", "id": "i3", "alt": "x", "src": {"theme": "logo"}},
                    {"type": "heading", "id": "h1", "level": 1, "text": "x",
                     "visible_if": {"list": SNIPPET}}
                ]]}]
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_block_document_names_its_snippets_assets_and_conditions_once() {
        let needs = extras_needs(&document(json!("default")));

        assert_eq!(needs.theme, ThemeNeed::Default);
        assert_eq!(needs.snippets, [SNIPPET]);
        assert_eq!(needs.assets, [ASSET]);
        let ids: Vec<&str> = needs.conditions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["s1", "h1"]);
    }

    #[test]
    fn the_theme_choice_follows_the_document() {
        assert_eq!(
            extras_needs(&document(json!("none"))).theme,
            ThemeNeed::None
        );
        assert_eq!(
            extras_needs(&document(json!({"id": SNIPPET}))).theme,
            ThemeNeed::Id(SNIPPET.to_owned())
        );
    }

    #[test]
    fn a_code_document_asks_for_nothing() {
        let doc = TemplateDocument::email_code(crate::EmailContent::default());

        assert_eq!(extras_needs(&doc), ExtrasNeeds::nothing());
    }

    #[test]
    fn loaded_values_become_extras_and_unreadable_ones_are_left_out() {
        let tokens = json!({"primary": "#112233", "text": "#222222", "background": "#ffffff"});
        let snippet = json!({"blocks": [{"type": "divider", "id": "d"}]});
        let bad = json!({"blocks": "nope"});
        let loaded = Loaded {
            theme: Some(("Brand", &tokens)),
            snippets: vec![(SNIPPET, &snippet), ("other", &bad)],
            assets: BTreeMap::from([(ASSET.to_owned(), "https://x.example/a/1.png".to_owned())]),
            conditions: BTreeMap::from([("s1".to_owned(), false)]),
        };

        let extras = render_extras(loaded);

        assert_eq!(
            extras.theme.as_ref().map(|theme| theme.primary.as_str()),
            Some("#112233")
        );
        assert_eq!(extras.theme_name.as_deref(), Some("Brand"));
        assert_eq!(extras.snippets.keys().collect::<Vec<_>>(), [SNIPPET]);
        assert_eq!(extras.asset_url(ASSET), Some("https://x.example/a/1.png"));
        assert_eq!(extras.conditions.get("s1"), Some(&false));

        let unreadable = Loaded {
            theme: Some(("Brand", &bad)),
            ..Loaded::default()
        };
        assert_eq!(render_extras(unreadable), RenderExtras::default());
    }
}
