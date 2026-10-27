//! `draft_template`: checks a drafted change to a template's document with the validation a
//! publish applies (the parse, then the renderer's check against the field catalog and the
//! effective theme, snippets and conditions) and returns it or the problems as codes and places
//! (architecture 13.2). It writes nothing: accepting the draft is a separate step that copies it
//! into the template's ordinary draft, and publishing or sending stays a human action.
//!
//! The template and the task come from the authorized request ([`DraftTarget::Template`]), never
//! from the model, which can only fill in a fragment of the content: the fields the task is for.
//! The fragment is merged onto the draft's own document, so the sender, the category and the
//! channel stay what they are.

use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::ToSchema;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_templates::{Issue, TemplateDocument, migrate_to_current};

use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::schema::tool_parameters;
use crate::auth::Need;
use crate::problem::ApiError;
use crate::templates::publish::{publish_inputs, publish_issues};
use crate::templates::{check_matches, parse_document};

pub(crate) struct DraftTemplate;

/// The most feedback lines one refusal carries.
const MAX_LINES: usize = 50;
/// The fewest and the most subjects a `subject_variants` task offers.
const VARIANTS: std::ops::RangeInclusive<usize> = 2..=5;
/// The longest subject, in characters.
const MAX_SUBJECT_CHARS: usize = 300;
/// The longest language tag, in characters.
const MAX_LANG_CHARS: usize = 35;

/// What the marketer asks the assistant to do to a template.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TemplateTask {
    /// Write or rewrite the text of the message.
    Copy,
    /// Offer two to five subjects for the message.
    SubjectVariants,
    /// Arrange the sections and blocks of a block email.
    BlockLayout,
    /// Translate the message into another language and set its `lang`.
    Translation,
}

impl TemplateTask {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::SubjectVariants => "subject_variants",
            Self::BlockLayout => "block_layout",
            Self::Translation => "translation",
        }
    }
}

/// The arguments of `draft_template`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftTemplateArgs {
    /// For the tasks `copy`, `block_layout` and `translation`: the content fields to change, each
    /// whole, such as `{"subject": "...", "html": "...", "text": "..."}` for a code email or
    /// `{"subject": "...", "sections": [...]}` for a block email. A translation also sets `lang`.
    /// Fields it leaves out stay as they are.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub(crate) fragment: Option<Value>,
    /// For the task `subject_variants`: two to five subjects, the best first.
    #[serde(default)]
    pub(crate) subject_variants: Option<Vec<String>>,
}

/// A drafted template document, before the renderer has looked at it.
#[derive(Debug)]
pub(crate) struct Drafted {
    /// The whole document at the current schema, in the form a draft is stored.
    pub(crate) document: Value,
    pub(crate) doc: TemplateDocument,
    /// The subjects a `subject_variants` task offered; the first is in the document.
    pub(crate) subject_variants: Vec<String>,
}

/// The key of the content a document keeps its text in.
fn content_key(document: &Value) -> &'static str {
    if document.get("mode").and_then(Value::as_str) == Some("blocks") {
        "blocks"
    } else {
        "email"
    }
}

/// The content fields a task may set, or none when the task is not for the document's mode.
fn allowed_keys(task: TemplateTask, key: &str) -> Option<&'static [&'static str]> {
    match (task, key) {
        (TemplateTask::Copy, "email") => Some(&["subject", "html", "text"]),
        (TemplateTask::Translation, "email") => Some(&["subject", "html", "text", "lang"]),
        (TemplateTask::Copy, _) => Some(&["subject", "preheader", "sections"]),
        (TemplateTask::BlockLayout, "blocks") => Some(&["sections"]),
        (TemplateTask::Translation, _) => Some(&["subject", "preheader", "sections", "lang"]),
        (TemplateTask::BlockLayout | TemplateTask::SubjectVariants, _) => None,
    }
}

fn subject_lines(list: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    if !VARIANTS.contains(&list.len()) {
        lines.push("variants_count at /subject_variants".to_owned());
    }
    for (index, subject) in list.iter().enumerate() {
        let length = subject.trim().chars().count();
        if length == 0 || subject.chars().count() > MAX_SUBJECT_CHARS {
            lines.push(format!("subject_invalid at /subject_variants/{index}"));
        }
    }
    lines
}

fn lang_is_valid(lang: &Value) -> bool {
    lang.as_str().is_some_and(|tag| {
        (2..=MAX_LANG_CHARS).contains(&tag.chars().count())
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && tag.starts_with(|c: char| c.is_ascii_alphabetic())
    })
}

/// The fields of a fragment the task may not set, and what else is wrong with it.
fn fragment_lines(
    task: TemplateTask,
    allowed: &[&str],
    fragment: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let mut lines: Vec<String> = fragment
        .keys()
        .filter(|name| !allowed.contains(&name.as_str()))
        .map(|name| format!("fragment_field_not_allowed at /fragment/{name}"))
        .collect();
    if fragment.is_empty() {
        lines.push("fragment_missing at /fragment".to_owned());
    }
    if task == TemplateTask::Translation && !fragment.get("lang").is_some_and(lang_is_valid) {
        lines.push("lang_invalid at /fragment/lang".to_owned());
    }
    lines
}

/// The pure part of the tool: the arguments against the schema and the task, merged onto `base`,
/// then read as a template document. It returns the document in its stored form, or the feedback
/// lines. The golden-prompt harness runs it without a database.
pub(crate) fn apply(
    task: TemplateTask,
    base: &Value,
    arguments: &Value,
) -> Result<Drafted, Vec<String>> {
    let schema = DraftTemplate::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let args: DraftTemplateArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    let key = content_key(base);
    let mut merged = base.clone();
    let document = merged.as_object_mut().ok_or_else(arguments_invalid_lines)?;
    if !document.get(key).is_some_and(Value::is_object) {
        document.insert(key.to_owned(), json!({}));
    }
    let content = document
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(arguments_invalid_lines)?;

    let mut subject_variants = Vec::new();
    if task == TemplateTask::SubjectVariants {
        if args.fragment.is_some() {
            return Err(vec!["fragment_not_allowed at /fragment".to_owned()]);
        }
        let list = args.subject_variants.unwrap_or_default();
        let lines = subject_lines(&list);
        if !lines.is_empty() {
            return Err(lines);
        }
        content.insert("subject".to_owned(), Value::from(list[0].clone()));
        subject_variants = list;
    } else {
        if args.subject_variants.is_some() {
            return Err(vec![
                "subject_variants_not_allowed at /subject_variants".to_owned(),
            ]);
        }
        let Some(allowed) = allowed_keys(task, key) else {
            return Err(vec!["task_not_for_mode at /fragment".to_owned()]);
        };
        let Some(Value::Object(fragment)) = args.fragment else {
            return Err(vec!["fragment_missing at /fragment".to_owned()]);
        };
        let lines = fragment_lines(task, allowed, &fragment);
        if !lines.is_empty() {
            return Err(lines);
        }
        content.extend(fragment);
    }

    let doc = parse_document(&merged).map_err(|error| {
        vec![format!(
            "{} at /{key}",
            error.code().unwrap_or("template_malformed")
        )]
    })?;
    let document =
        migrate_to_current(merged).map_err(|_| vec!["unsupported_schema at /schema".to_owned()])?;
    Ok(Drafted {
        document,
        doc,
        subject_variants,
    })
}

/// The blocking issues as feedback lines: the code, the place and, for a merge field, its name.
/// The name is the one the draft spelled; it is never a value of a person.
pub(crate) fn issue_lines(issues: &[Issue]) -> Vec<String> {
    issues
        .iter()
        .filter(|issue| issue.is_blocking())
        .take(MAX_LINES)
        .map(|issue| match &issue.field {
            Some(field) => format!("{} at {} | field {field}", issue.code, issue.at),
            None => format!("{} at {}", issue.code, issue.at),
        })
        .collect()
}

impl DraftTemplate {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<DraftTemplateArgs>()
    }
}

impl AiTool<ToolContext> for DraftTemplate {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "draft_template"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Draft
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Proposes a change to the template's content: a `fragment` of content fields for \
                 the tasks copy, block_layout and translation, or `subject_variants` for the task \
                 subject_variants. The result is checked with the validation a publish applies; \
                 the problems come back with the place of each if it is refused. Nothing is \
                 saved, sent or published."
                    .into(),
            ),
            parameters: Self::parameters()?,
        })
    }

    fn call<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError<ApiError>>> {
        Box::pin(async move {
            authorize_again(context, Need::Edit).await?;
            let DraftTarget::Template {
                template,
                task,
                base,
            } = &context.target
            else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            let drafted = apply(*task, base, arguments).map_err(ToolError::Invalid)?;
            check_matches(template, &drafted.doc).map_err(|error| {
                ToolError::Invalid(vec![format!(
                    "{} at /",
                    error.code().unwrap_or("template_mismatch")
                )])
            })?;
            let render = context
                .state
                .render_pool()
                .ok_or_else(|| ToolError::Failed(ApiError::render_unavailable()))?;
            let (catalog, inputs) = {
                let mut connection = context
                    .state
                    .pool
                    .acquire()
                    .await
                    .map_err(|_| ToolError::Failed(ApiError::internal()))?;
                publish_inputs(
                    &context.state,
                    &mut connection,
                    context.workspace,
                    &drafted.doc,
                )
                .await
                .map_err(ToolError::Failed)?
            };
            let issues = publish_issues(render, &drafted.doc, &catalog, &inputs)
                .await
                .map_err(ToolError::Failed)?;
            let lines = issue_lines(&issues);
            if !lines.is_empty() {
                return Err(ToolError::Invalid(lines));
            }
            Ok(ToolOutput::Draft(json!({
                "document": drafted.document,
                "subject_variants": drafted.subject_variants,
            })))
        })
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::people::TraitType;
    use zirv_flow_templates::{FieldCatalog, validate};

    use super::*;

    const SENDER: &str = "0192b1a4-7c3e-7000-8000-00000000a001";

    fn code_doc() -> Value {
        json!({"schema": 2, "channel": "email", "mode": "code", "email": {
            "subject": "Hello", "sender": SENDER, "lang": "en", "category": "marketing",
            "html": "<p>Hi</p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
            "text": "Hi {{ system.unsubscribe_url }}"
        }})
    }

    fn blocks_doc() -> Value {
        json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
            "subject": "Hello", "sender": SENDER, "lang": "en", "category": "marketing",
            "sections": [{"id": "s1", "columns": [[
                {"type": "heading", "id": "h1", "level": 1, "text": "Hi"}
            ]]}]
        }})
    }

    fn catalog() -> FieldCatalog {
        let mut catalog = FieldCatalog::default();
        catalog.person.insert("plan".into(), TraitType::String);
        catalog.with_reserved_email()
    }

    #[test]
    fn the_tool_drafts_and_offers_a_fragment_or_subjects() {
        let tool = DraftTemplate;
        assert_eq!(tool.name(), "draft_template");
        assert_eq!(tool.effect(), ToolEffect::Draft);
        let schema = tool.spec().unwrap().parameters;

        let properties = &schema.as_value()["properties"];
        assert_eq!(properties["fragment"]["type"], "object");
        assert_eq!(properties["subject_variants"]["type"], "array");
        assert_eq!(schema.validate_value(&json!({"fragment": {}})), Ok(()));
        assert!(schema.validate_value(&json!({"fragment": 3})).is_err());
    }

    #[test]
    fn a_copy_fragment_changes_only_the_fields_it_names() {
        let arguments = json!({"fragment": {"subject": "Welcome, {{ person.plan }}"}});

        let drafted = apply(TemplateTask::Copy, &code_doc(), &arguments).unwrap();

        assert_eq!(
            drafted.document["email"]["subject"],
            "Welcome, {{ person.plan }}"
        );
        assert_eq!(drafted.document["email"]["sender"], SENDER);
        assert_eq!(
            drafted.document["email"]["html"],
            code_doc()["email"]["html"]
        );
        assert!(drafted.subject_variants.is_empty());
        assert!(
            validate(&drafted.doc, &catalog())
                .iter()
                .all(|issue| !issue.is_blocking())
        );
    }

    #[test]
    fn subject_variants_set_the_first_and_return_all() {
        let arguments = json!({"subject_variants": ["One", "Two", "Three"]});

        let drafted = apply(TemplateTask::SubjectVariants, &code_doc(), &arguments).unwrap();

        assert_eq!(drafted.document["email"]["subject"], "One");
        assert_eq!(drafted.subject_variants, ["One", "Two", "Three"]);
        for bad in [
            json!({"subject_variants": ["Only one"]}),
            json!({"subject_variants": ["a", "b", "c", "d", "e", "f"]}),
            json!({"subject_variants": ["a", " "]}),
        ] {
            assert!(
                apply(TemplateTask::SubjectVariants, &code_doc(), &bad).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            apply(
                TemplateTask::SubjectVariants,
                &code_doc(),
                &json!({"fragment": {"subject": "x"}, "subject_variants": ["a", "b"]})
            )
            .unwrap_err(),
            ["fragment_not_allowed at /fragment"]
        );
    }

    #[test]
    fn a_block_layout_replaces_the_sections_of_a_block_email_only() {
        let arguments = json!({"fragment": {"sections": [{"id": "s9", "columns": [[
            {"type": "divider", "id": "d1"}]]}]}});

        let drafted = apply(TemplateTask::BlockLayout, &blocks_doc(), &arguments).unwrap();

        assert_eq!(drafted.document["blocks"]["sections"][0]["id"], "s9");
        assert_eq!(drafted.document["blocks"]["subject"], "Hello");
        assert_eq!(
            apply(TemplateTask::BlockLayout, &code_doc(), &arguments).unwrap_err(),
            ["task_not_for_mode at /fragment"]
        );
    }

    #[test]
    fn a_translation_sets_the_language_and_a_copy_may_not() {
        let translated = json!({"fragment": {"subject": "Hej", "lang": "da"}});

        let drafted = apply(TemplateTask::Translation, &code_doc(), &translated).unwrap();

        assert_eq!(drafted.document["email"]["lang"], "da");
        assert_eq!(drafted.document["email"]["subject"], "Hej");
        assert_eq!(
            apply(TemplateTask::Copy, &code_doc(), &translated).unwrap_err(),
            ["fragment_field_not_allowed at /fragment/lang"]
        );
        for bad in [json!({"subject": "Hej"}), json!({"lang": "not a tag!"})] {
            assert_eq!(
                apply(
                    TemplateTask::Translation,
                    &code_doc(),
                    &json!({"fragment": bad})
                )
                .unwrap_err(),
                ["lang_invalid at /fragment/lang"]
            );
        }
    }

    #[test]
    fn the_sender_the_category_and_the_mode_cannot_be_set() {
        for field in ["sender", "category", "mode", "syntax", "theme"] {
            for (task, document) in [
                (TemplateTask::Copy, code_doc()),
                (TemplateTask::Translation, code_doc()),
                (TemplateTask::Copy, blocks_doc()),
                (TemplateTask::Translation, blocks_doc()),
                (TemplateTask::BlockLayout, blocks_doc()),
            ] {
                let mut arguments = json!({"fragment": {field: "x"}});
                if task == TemplateTask::Translation {
                    arguments["fragment"]["lang"] = json!("da");
                }
                assert_eq!(
                    apply(task, &document, &arguments).unwrap_err(),
                    [format!("fragment_field_not_allowed at /fragment/{field}")]
                );
            }
        }
        assert_eq!(
            apply(TemplateTask::Copy, &code_doc(), &json!({"fragment": {}})).unwrap_err(),
            ["fragment_missing at /fragment"]
        );
        assert_eq!(
            apply(TemplateTask::Copy, &code_doc(), &json!({})).unwrap_err(),
            ["fragment_missing at /fragment"]
        );
    }

    #[test]
    fn a_block_the_document_does_not_know_is_a_code_and_a_place() {
        let arguments = json!({"fragment": {"sections": [{"id": "s1", "columns": [[
            {"type": "teleport", "id": "t1"}]]}]}});

        let lines = apply(TemplateTask::Copy, &blocks_doc(), &arguments).unwrap_err();

        assert_eq!(lines, ["template_unknown_block at /blocks"]);
    }

    #[test]
    fn a_merge_field_the_catalog_does_not_know_is_feedback_with_its_name() {
        let arguments = json!({"fragment": {"html": "<p>{{ person.nope }}</p><a href=\"{{ system.unsubscribe_url }}\">u</a>"}});
        let drafted = apply(TemplateTask::Copy, &code_doc(), &arguments).unwrap();

        let lines = issue_lines(&validate(&drafted.doc, &catalog()));

        assert!(
            lines
                .iter()
                .any(|line| line
                    .starts_with("unknown_merge_field at /email/html | field person.nope")),
            "{lines:?}"
        );
    }
}
