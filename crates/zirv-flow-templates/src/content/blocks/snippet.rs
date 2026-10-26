//! Snippet content (architecture 11.3): a block list kept outside any template, and its pure
//! validator. The crate stores nothing; a caller loads a snippet and passes it in
//! [`crate::RenderExtras`].

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};

use super::check::{Field, block_fields, check_field_in, check_texts};
use super::model::{Block, Section, block_at, check_block_numbers, check_id};
use crate::issue::{FieldCatalog, Issue, code};
use crate::sandbox::{FUEL, environment};
use crate::validate::sample_context;

/// Blocks in one snippet.
pub const MAX_SNIPPET_BLOCKS: usize = 50;

/// The content of a snippet: blocks, with no sections around them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetContent {
    #[serde(default)]
    pub blocks: Vec<Block>,
    /// Never allowed. The field exists so that a value that carries sections gets the typed
    /// `snippet_section` issue and not an opaque unknown-field error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
}

/// Check a snippet (the `theme.*` names and the theme logo are accepted: whether a theme is in
/// effect is the template's business): it holds no section, no snippet block and no `visible_if`; its ids, numbers,
/// URLs, rich text and raw HTML pass the checks a template's blocks pass, and every merge field
/// is one the catalog knows. A snippet need not hold an unsubscribe link, because the template
/// that uses it is checked as a whole. The result is in document order.
pub fn validate_snippet(snippet: &SnippetContent, catalog: &FieldCatalog) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !snippet.sections.is_empty() {
        issues.push(
            Issue::new(code::SNIPPET_SECTION, "/sections")
                .with_message("a snippet is a list of blocks, with no section"),
        );
    }
    if snippet.blocks.len() > MAX_SNIPPET_BLOCKS {
        issues.push(
            Issue::new(code::BLOCKS_LIMIT, "/blocks")
                .with_message(format!("at most {MAX_SNIPPET_BLOCKS} blocks")),
        );
        return issues;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for block in &snippet.blocks {
        check_id(block.id(), &mut seen, &mut issues);
        check_block_numbers(block, &mut issues);
        if matches!(block, Block::Snippet(_)) {
            issues.push(
                Issue::new(code::SNIPPET_NESTED, block_at(block.id(), None))
                    .with_message("a snippet holds no snippet block"),
            );
        }
        if block.visible_if().is_some() {
            issues.push(
                Issue::new(
                    code::SNIPPET_CONDITION,
                    block_at(block.id(), Some("visible_if")),
                )
                .with_message("a snippet holds no visibility condition"),
            );
        }
    }
    let mut fields: Vec<Field<'_>> = Vec::new();
    for block in &snippet.blocks {
        block_fields(block, &mut fields);
    }
    check_texts(&fields, &mut issues);
    if issues
        .iter()
        .any(|issue| matches!(issue.code, code::BLOCKS_LIMIT | code::CONTENT_TOO_LARGE))
    {
        return issues;
    }
    let mut env = environment();
    env.set_fuel(Some(FUEL));
    let sample = sample_context(catalog);
    for field in &fields {
        check_field_in(&env, catalog, true, &sample, field, &mut issues);
    }
    issues
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn catalog() -> FieldCatalog {
        FieldCatalog {
            person: BTreeMap::from([("name".to_owned(), TraitType::String)]),
            ..FieldCatalog::default()
        }
    }

    fn snippet(value: serde_json::Value) -> SnippetContent {
        serde_json::from_value(value).unwrap()
    }

    fn codes(issues: &[Issue]) -> Vec<(&'static str, &str)> {
        issues
            .iter()
            .map(|issue| (issue.code, issue.at.as_str()))
            .collect()
    }

    #[test]
    fn a_clean_snippet_has_no_issue_and_needs_no_unsubscribe_link() {
        let content = snippet(json!({"blocks": [
            {"type": "heading", "id": "h", "level": 2, "text": "Hi {{ person.name }}"},
            {"type": "text", "id": "t", "content": "<p>Thanks</p>"}
        ]}));
        assert_eq!(validate_snippet(&content, &catalog()), Vec::new());
    }

    #[test]
    fn a_section_a_snippet_block_and_a_condition_are_refused() {
        let content = snippet(json!({
        "sections": [{"id": "s", "columns": [[]]}],
        "blocks": [
            {"type": "snippet", "id": "n", "snippet_id": "x"},
            {"type": "spacer", "id": "p", "height": 5, "visible_if": {"all": []}}
        ]}));
        let issues = validate_snippet(&content, &catalog());
        assert_eq!(
            codes(&issues),
            [
                ("snippet_section", "/sections"),
                ("snippet_nested", "/blocks/block/n"),
                ("snippet_condition", "/blocks/block/p/visible_if"),
            ]
        );
        assert!(issues.iter().all(Issue::is_blocking));
    }

    #[test]
    fn an_unknown_merge_field_and_a_bad_url_are_reported_with_block_paths() {
        let content = snippet(json!({"blocks": [
            {"type": "heading", "id": "h", "level": 1, "text": "{{ person.nope }}"},
            {"type": "button", "id": "b", "label": "Go", "url": "ftp://x.example"}
        ]}));
        let issues = validate_snippet(&content, &catalog());
        assert_eq!(
            codes(&issues),
            [
                ("invalid_url", "/blocks/block/b/url"),
                ("unknown_merge_field", "/blocks/block/h/text"),
            ]
        );
        assert_eq!(issues[1].field.as_deref(), Some("person.nope"));
    }

    #[test]
    fn the_block_count_and_a_rejected_rich_text_are_checked() {
        let many = SnippetContent {
            blocks: (0..=MAX_SNIPPET_BLOCKS)
                .map(|n| {
                    serde_json::from_value(json!({"type": "divider", "id": format!("d{n}")}))
                        .unwrap()
                })
                .collect(),
            sections: Vec::new(),
        };
        assert_eq!(
            codes(&validate_snippet(&many, &catalog())),
            [("blocks_limit", "/blocks")]
        );
        let rich = snippet(json!({"blocks": [
            {"type": "text", "id": "t", "content": "<script>x</script>"}]}));
        assert_eq!(
            validate_snippet(&rich, &catalog())[0].code,
            "sanitizer_rejected"
        );
    }

    #[test]
    fn the_known_theme_names_are_accepted() {
        for name in crate::context::THEME_FIELDS {
            let content = snippet(json!({"blocks": [
                {"type": "heading", "id": "h", "level": 2, "text": format!("{{{{ theme.{name} | default(\"\") }}}}")}
            ]}));
            assert_eq!(validate_snippet(&content, &catalog()), Vec::new(), "{name}");
        }
    }

    #[test]
    fn an_unknown_theme_name_is_an_unknown_merge_field() {
        let content = snippet(json!({"blocks": [
            {"type": "heading", "id": "h", "level": 2, "text": "{{ theme.nope }}"}]}));
        let issues = validate_snippet(&content, &catalog());
        assert_eq!(
            codes(&issues),
            [("unknown_merge_field", "/blocks/block/h/text")]
        );
        assert_eq!(issues[0].field.as_deref(), Some("theme.nope"));
    }

    #[test]
    fn an_image_with_the_theme_logo_is_accepted() {
        let content = snippet(json!({"blocks": [
            {"type": "image", "id": "i", "src": {"theme": "logo"}, "alt": "Logo"}]}));
        assert_eq!(validate_snippet(&content, &catalog()), Vec::new());
    }
}
