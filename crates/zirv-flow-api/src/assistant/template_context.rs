//! What the assistant tells the model about a template (architecture 13.3, 13.4): the fixed
//! instructions, the format of the content it may change, and the context of the one template.
//!
//! The context is sample-only. It holds the template's document, the names of the merge fields
//! the workspace offers (trait keys and event property names with their types), the names of the
//! themes and snippets, and the task. It never holds a person, an address or a value of a trait:
//! a preview of the result is rendered for the sample person on the server, and the model sees
//! none of it.
//!
//! The one place a person's value can hide in a document is a `visible_if`, a segment condition
//! with the literals a marketer typed ("email is jane@..."). Under schema-only privacy every
//! `visible_if` goes through the segment assistant's masker (a literal compared against a trait
//! that is not declared non-personal becomes a numbered placeholder) and the placeholders are put
//! back into the model's fragment, where one that moved or was made up is an issue to repair. With
//! the workspace's opt-in the document is sent as it is, fenced as personal data. The authored
//! copy is the object of the task and is workspace content: it is sent either way.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_ai::{Content, Privacy};

use super::prompt::omitted;
use super::segment_context::{ClearKeys, Placeholders, mask_many, within_walk};
use super::tools::draft_template::TemplateTask;
use crate::problem::ApiError;

/// The most bytes of template document the context carries.
pub(crate) const MAX_DOCUMENT_BYTES: usize = 96 * 1024;
/// The most merge fields of each kind the context lists.
const MAX_FIELDS: usize = 500;
/// The most themes and the most snippets the context lists.
const MAX_NAMED: usize = 200;

pub(crate) const TEMPLATE_INSTRUCTIONS: &str = "\
You help a marketer write a message template. Answer by calling the draft_template tool exactly \
once, and by nothing else.

You are given the template's current document and the task. Change only what the task concerns \
and leave every other field out of your answer: what you leave out stays as it is. Write for the \
reader of the message, in the language the request asks for, or the document's own language.

Merge fields are written {{ person.<trait> }} and {{ event.<property> }}, and only the ones listed \
below exist: never invent one. Keep every merge field, link, id and {{ system.* }} placeholder of \
the existing text that you do not have a reason to change. A marketing message keeps a link to \
{{ system.unsubscribe_url }}. You cannot change the sender, the category or the theme, and you \
cannot send or publish anything.

If your answer is refused, the problems are listed with the place of each: fix exactly those and \
call the tool again.";

pub(crate) const TEMPLATE_REFERENCE: &str = "\
The tool takes a `fragment` of content fields, or, for the task subject_variants, \
`subject_variants`.

A code email keeps its content in `email`: subject (a template string), html, text (the plain \
text part). Its fragment may hold subject, html and text.

A block email keeps its content in `blocks`: subject, preheader, lang and sections. A section is \
{\"id\": \"s1\", \"columns\": [[block, ...], ...]}: one to three columns, each a list of blocks, \
and ids are unique and use letters, digits, - and _. A block has an id and a type:
- {\"type\": \"heading\", \"id\": \"h1\", \"level\": 1, \"text\": \"...\"}
- {\"type\": \"text\", \"id\": \"t1\", \"content\": \"...\"} (limited rich text)
- {\"type\": \"button\", \"id\": \"b1\", \"label\": \"...\", \"url\": \"https://...\"}
- {\"type\": \"image\", \"id\": \"i1\", \"src\": {\"url\": \"https://...\"}, \"alt\": \"...\"}
- {\"type\": \"divider\", \"id\": \"d1\"} and {\"type\": \"spacer\", \"id\": \"p1\", \"height\": 16}
- {\"type\": \"snippet\", \"id\": \"n1\", \"snippet_id\": \"<id of a snippet listed below>\"}
Its fragment may hold subject, preheader and sections; sections are replaced whole.

Tasks:
- copy: rewrite the text as the request says.
- subject_variants: two to five subject lines for the message, the best first, as \
`subject_variants`. Give no fragment.
- block_layout: arrange the sections and blocks of a block email, as a fragment with `sections`. \
Keep the text of the blocks you keep.
- translation: translate everything a reader sees (subject, preheader, headings, text, button \
labels, image alt text) and set `lang` in the fragment to the language tag of the translation, \
such as da or en-GB. Keep merge fields, links and ids exactly.";

/// What the model is told about one template.
pub(crate) struct TemplateContext<'a> {
    pub(crate) name: &'a str,
    pub(crate) channel: &'a str,
    pub(crate) category: &'a str,
    /// The draft's document.
    pub(crate) document: &'a Value,
    /// Trait key and type name.
    pub(crate) traits: Vec<(String, String)>,
    /// Event property name and type name.
    pub(crate) event_properties: Vec<(String, String)>,
    pub(crate) themes: Vec<(Uuid, String)>,
    pub(crate) snippets: Vec<(Uuid, String)>,
    pub(crate) task: TemplateTask,
}

fn list(title: &str, entries: &[(String, String)], notes: &mut Vec<String>) -> String {
    let mut text = format!("{title}:\n");
    if entries.is_empty() {
        text.push_str("(none)\n");
    }
    for (name, kind) in entries.iter().take(MAX_FIELDS) {
        text.push_str(&format!("- {name} ({kind})\n"));
    }
    omitted(notes, title, entries.len(), MAX_FIELDS);
    text
}

fn named(title: &str, entries: &[(Uuid, String)], notes: &mut Vec<String>) -> String {
    let mut text = format!("{title}:\n");
    if entries.is_empty() {
        text.push_str("(none)\n");
    }
    for (id, name) in entries.iter().take(MAX_NAMED) {
        text.push_str(&format!("- {name} ({id})\n"));
    }
    omitted(notes, title, entries.len(), MAX_NAMED);
    text
}

/// The pointers, below `base`, of every `visible_if` of the sections and of the blocks in their
/// columns. `sections` is the list of sections at `base`.
fn condition_pointers(sections: Option<&Value>, base: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, section) in sections
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("{base}/{index}");
        if section
            .get("visible_if")
            .is_some_and(|value| !value.is_null())
        {
            found.push(format!("{at}/visible_if"));
        }
        for (column, blocks) in section
            .get("columns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            for (position, block) in blocks.as_array().into_iter().flatten().enumerate() {
                if block
                    .get("visible_if")
                    .is_some_and(|value| !value.is_null())
                {
                    found.push(format!("{at}/columns/{column}/{position}/visible_if"));
                }
            }
        }
    }
    found
}

/// The document with the personal literals of its conditions replaced by placeholders, and the
/// table to put them back. A condition deeper or larger than the masking walk covers is refused:
/// its literals could not all be found.
fn mask_document(document: &Value, clear: &ClearKeys) -> Result<(Value, Placeholders), ApiError> {
    let pointers = condition_pointers(document.pointer("/blocks/sections"), "/blocks/sections");
    for pointer in &pointers {
        if document
            .pointer(pointer)
            .is_some_and(|condition| !within_walk(condition, 1, &mut 0))
        {
            return Err(ApiError::ai_template_unsupported(format!(
                "the condition at {pointer} is too deep or too large for the assistant;                  simplify that condition first"
            )));
        }
    }
    let conditions: Vec<Value> = pointers
        .iter()
        .filter_map(|pointer| document.pointer(pointer).cloned())
        .collect();
    let (masked, table) = mask_many(&conditions, clear);
    let mut out = document.clone();
    for (pointer, condition) in pointers.iter().zip(masked) {
        if let Some(target) = out.pointer_mut(pointer) {
            *target = condition;
        }
    }
    Ok((out, table))
}

/// Puts the literals the model was not shown back into its `fragment`, wherever its conditions
/// are, and names (as `code at <pointer>`) every placeholder that is not where it was sent or was
/// made up, so the model repairs it.
pub(crate) fn restore_fragment(table: &Placeholders, arguments: &mut Value) -> Vec<String> {
    let Some(fragment) = arguments.get_mut("fragment") else {
        return Vec::new();
    };
    for pointer in condition_pointers(fragment.get("sections"), "/sections") {
        if let Some(condition) = fragment.pointer_mut(&pointer) {
            table.restore(condition);
        }
    }
    // What is still a placeholder after that is one the model moved or made up.
    table
        .restore(fragment)
        .into_iter()
        .map(|line| line.replacen(" at ", " at /fragment", 1))
        .collect()
}

/// The context as request content. Under [`Privacy::SchemaOnly`] the document's conditions are
/// masked and the content is plain schema text, with the table to restore the model's answer;
/// with the opt-in the document is sent as it is, as personal data. A document over
/// [`MAX_DOCUMENT_BYTES`] is refused: it is cut nowhere, because a half document is not the one
/// the change would be merged onto.
pub(crate) fn render_template_context(
    context: &TemplateContext<'_>,
    clear: &ClearKeys,
    privacy: Privacy,
) -> Result<(Vec<Content>, Placeholders), ApiError> {
    if context.document.to_string().len() > MAX_DOCUMENT_BYTES {
        return Err(ApiError::ai_template_too_large());
    }
    let (document, table) = match privacy {
        Privacy::SchemaOnly => mask_document(context.document, clear)?,
        Privacy::PersonalAllowed => (context.document.clone(), Placeholders::default()),
    };
    let mut notes = Vec::new();
    let mut text = format!(
        "The template is named {}. It is a {} message, {}.\n",
        context.name, context.channel, context.category
    );
    text.push_str(&format!("The task is {}.\n", context.task.as_str()));
    text.push_str(&list("Person merge fields", &context.traits, &mut notes));
    text.push_str(&list(
        "Event merge fields",
        &context.event_properties,
        &mut notes,
    ));
    text.push_str(&named("Themes", &context.themes, &mut notes));
    text.push_str(&named("Snippets", &context.snippets, &mut notes));
    for note in notes {
        text.push_str(&note);
        text.push('\n');
    }
    let document = format!("The current document: {document}\n");
    let document = match privacy {
        Privacy::SchemaOnly => Content::Schema(document),
        Privacy::PersonalAllowed => Content::Personal(document),
    };
    Ok((vec![Content::Schema(text), document], table))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn context<'a>(document: &'a Value, task: TemplateTask) -> TemplateContext<'a> {
        TemplateContext {
            name: "Welcome",
            channel: "email",
            category: "marketing",
            document,
            traits: vec![("plan".into(), "string".into())],
            event_properties: vec![("order_id".into(), "string".into())],
            themes: vec![(Uuid::nil(), "Brand".into())],
            snippets: vec![(Uuid::from_u128(7), "Footer note".into())],
            task,
        }
    }

    fn rendered(context: &TemplateContext<'_>) -> String {
        render_template_context(context, &ClearKeys::default(), Privacy::SchemaOnly)
            .unwrap()
            .0
            .iter()
            .map(Content::render)
            .collect()
    }

    #[test]
    fn the_context_names_the_fields_the_document_and_the_task_and_nothing_else() {
        let document = json!({"schema": 2, "email": {"subject": "Hi"}});

        let text = rendered(&context(&document, TemplateTask::Translation));

        for part in [
            "The task is translation.",
            "- plan (string)",
            "- order_id (string)",
            "- Brand (",
            "- Footer note (",
            "\"subject\":\"Hi\"",
        ] {
            assert!(text.contains(part), "{part} missing from {text}");
        }
    }

    #[test]
    fn every_part_is_schema_text_never_fenced_personal_data() {
        let document = json!({"schema": 2});

        let (parts, _) = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::SchemaOnly,
        )
        .unwrap();

        assert!(parts.iter().all(|part| matches!(part, Content::Schema(_))));
    }

    #[test]
    fn a_document_over_the_limit_is_refused_not_cut() {
        let document = json!({"email": {"html": "x".repeat(MAX_DOCUMENT_BYTES)}});

        let error = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::SchemaOnly,
        )
        .err()
        .unwrap();

        assert_eq!(error.code(), Some("ai_template_too_large"));
    }

    #[test]
    fn a_long_list_is_capped_with_a_note() {
        let document = json!({});
        let mut many = context(&document, TemplateTask::Copy);
        many.traits = (0..MAX_FIELDS + 3)
            .map(|n| (format!("t{n}"), "string".to_owned()))
            .collect();

        let text = rendered(&many);

        assert!(
            text.contains("3 Person merge fields were left out"),
            "{text}"
        );
        assert!(!text.contains(&format!("t{}", MAX_FIELDS + 2)));
    }

    #[test]
    fn the_instructions_fix_the_boundaries_of_the_tool() {
        assert!(TEMPLATE_INSTRUCTIONS.contains("draft_template tool exactly once"));
        assert!(TEMPLATE_INSTRUCTIONS.contains("cannot send or publish"));
        for task in ["copy", "subject_variants", "block_layout", "translation"] {
            assert!(TEMPLATE_REFERENCE.contains(&format!("- {task}:")), "{task}");
        }
    }

    fn conditioned() -> Value {
        json!({"schema": 2, "mode": "blocks", "blocks": {"sections": [
            {"id": "s1", "visible_if": {"trait": "email", "op": "eq", "value": "jane@example.org"},
             "columns": [[
                {"type": "text", "id": "t1", "content": "<p>Hello</p>",
                 "visible_if": {"trait": "country", "op": "eq", "value": "DK"}}
            ]]}
        ]}})
    }

    #[test]
    fn schema_only_masks_the_conditions_and_the_fragment_gets_them_back() {
        let document = conditioned();
        let (parts, table) = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::SchemaOnly,
        )
        .unwrap();
        let text: String = parts.iter().map(Content::render).collect();
        assert!(
            !text.contains("jane@example.org") && !text.contains("\"DK\""),
            "{text}"
        );
        assert!(
            text.contains("(kept:1)") && text.contains("(kept:2)"),
            "{text}"
        );
        assert!(text.contains("<p>Hello</p>"), "the authored copy stays");

        let mut answer = json!({"fragment": {"sections": [
            {"id": "s1", "visible_if": {"trait": "email", "op": "eq", "value": "(kept:1)"},
             "columns": [[{"type": "text", "id": "t1", "content": "<p>Hi</p>",
                "visible_if": {"trait": "country", "op": "eq", "value": "(kept:2)"}}]]}
        ]}});
        assert_eq!(restore_fragment(&table, &mut answer), Vec::<String>::new());
        let section = &answer["fragment"]["sections"][0];
        assert_eq!(section["visible_if"]["value"], "jane@example.org");
        assert_eq!(section["columns"][0][0]["visible_if"]["value"], "DK");
    }

    #[test]
    fn a_placeholder_that_moved_or_was_made_up_is_named_for_repair() {
        let document = conditioned();
        let (_, table) = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::SchemaOnly,
        )
        .unwrap();
        let mut answer = json!({"fragment": {"subject": "(kept:9)", "sections": [
            {"id": "s1", "visible_if": {"trait": "country", "op": "eq", "value": "(kept:1)"},
             "columns": [[{"type": "text", "id": "t1", "content": "(kept:2)"}]]}
        ]}});

        let mut lines = restore_fragment(&table, &mut answer);
        lines.sort();

        assert_eq!(
            lines,
            [
                "placeholder_invented at /fragment/subject",
                "placeholder_moved at /fragment/sections/0/columns/0/0/content",
                "placeholder_moved at /fragment/sections/0/visible_if/value",
            ]
        );
    }

    #[test]
    fn with_the_opt_in_the_document_is_sent_as_it_is_fenced_as_personal_data() {
        let document = conditioned();

        let (parts, table) = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::PersonalAllowed,
        )
        .unwrap();

        assert!(table.is_empty());
        let literal = parts
            .iter()
            .find(|part| part.render().contains("jane@example.org"));
        assert!(matches!(literal, Some(Content::Personal(_))));
    }

    fn nested_nots(depth: usize) -> Value {
        let mut condition = json!({"trait": "email", "op": "eq", "value": "jane@example.org"});
        for _ in 0..depth {
            condition = json!({"not": condition});
        }
        condition
    }

    fn with_condition(condition: Value) -> Value {
        json!({"schema": 2, "mode": "blocks", "blocks": {"sections": [
            {"id": "s1", "columns": [[
                {"type": "text", "id": "t1", "content": "<p>Hello</p>", "visible_if": condition}
            ]]}
        ]}})
    }

    #[test]
    fn schema_only_refuses_a_condition_the_mask_walk_cannot_cover() {
        let wide = json!({"all": (0..=200)
            .map(|n| json!({"trait": "email", "op": "eq", "value": format!("a{n}@example.org")}))
            .collect::<Vec<_>>()});
        for (condition, covered) in [
            (nested_nots(23), true),
            (nested_nots(24), false),
            (wide, false),
        ] {
            let document = with_condition(condition);
            let result = render_template_context(
                &context(&document, TemplateTask::Copy),
                &ClearKeys::default(),
                Privacy::SchemaOnly,
            );
            match (covered, result) {
                (true, Ok(_)) => {}
                (false, Err(error)) => {
                    assert_eq!(error.code(), Some("ai_template_unsupported"));
                    assert!(
                        error
                            .detail()
                            .contains("/blocks/sections/0/columns/0/0/visible_if"),
                        "{}",
                        error.detail()
                    );
                }
                (covered, other) => panic!("covered {covered}: {:?}", other.is_ok()),
            }
        }
    }

    #[test]
    fn the_opt_in_sends_a_condition_beyond_the_walk() {
        let document = with_condition(nested_nots(30));

        let (parts, _) = render_template_context(
            &context(&document, TemplateTask::Copy),
            &ClearKeys::default(),
            Privacy::PersonalAllowed,
        )
        .unwrap();

        assert!(
            parts
                .iter()
                .any(|part| part.render().contains("jane@example.org"))
        );
    }
}
