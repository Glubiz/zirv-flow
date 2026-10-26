//! What the assistant tells the model about a template (architecture 13.3, 13.4): the fixed
//! instructions, the format of the content it may change, and the context of the one template.
//!
//! The context is sample-only. It holds the template's document, the names of the merge fields
//! the workspace offers (trait keys and event property names with their types), the names of the
//! themes and snippets, and the task. It never holds a person, an address or a value of a trait:
//! a preview of the result is rendered for the sample person on the server, and the model sees
//! none of it.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_ai::Content;

use super::prompt::omitted;
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

/// The context as request content, all of it schema text. A document over
/// [`MAX_DOCUMENT_BYTES`] is refused: it is cut nowhere, because a half document is not the one
/// the change would be merged onto.
pub(crate) fn render_template_context(
    context: &TemplateContext<'_>,
) -> Result<Vec<Content>, ApiError> {
    let document = context.document.to_string();
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(ApiError::ai_template_too_large());
    }
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
    text.push_str(&format!("The current document: {document}\n"));
    Ok(vec![Content::Schema(text)])
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
        render_template_context(context)
            .unwrap()
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

        let parts = render_template_context(&context(&document, TemplateTask::Copy)).unwrap();

        assert!(parts.iter().all(|part| matches!(part, Content::Schema(_))));
    }

    #[test]
    fn a_document_over_the_limit_is_refused_not_cut() {
        let document = json!({"email": {"html": "x".repeat(MAX_DOCUMENT_BYTES)}});

        let error = render_template_context(&context(&document, TemplateTask::Copy)).unwrap_err();

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
}
