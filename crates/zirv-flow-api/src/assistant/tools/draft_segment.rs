//! `draft_segment`: checks a drafted segment definition with the rules a builder save applies,
//! `parse_and_validate` against the workspace's catalog and then the compiler (which normalizes),
//! and returns it or the problems as codes and JSON pointers, which hold no value (architecture
//! 13.2). It writes nothing: the draft reaches the builder as an unsaved copy and the person's
//! Save is the only write.
//!
//! The segment a definition is for comes from the authorized request ([`DraftTarget::Segment`]),
//! never from the model. It is used for one check only: a segment may not refer to itself.

use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_segments::{Catalog, Definition, Issue, TraitStorage, compile, parse_and_validate};

use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::schema::tool_parameters;
use crate::assistant::segment_context::nodes;
use crate::problem::ApiError;
use crate::segments::catalog_of;

pub(crate) struct DraftSegment;

/// The arguments of `draft_segment`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftSegmentArgs {
    /// The whole segment definition, version 1: the root node with `"v": 1` beside its key.
    #[schema(value_type = Object)]
    pub(crate) definition: Value,
}

/// An issue as a feedback line: its code and the JSON pointer of the node or key it concerns.
pub(crate) fn line(code: &str, pointer: &str) -> String {
    if pointer.is_empty() {
        format!("{code} at the definition root")
    } else {
        format!("{code} at {pointer}")
    }
}

fn lines_of(issues: &[Issue]) -> Vec<String> {
    issues
        .iter()
        .map(|issue| line(issue.code, &issue.pointer))
        .collect()
}

/// The pointers of the `segment` nodes that name `own`.
fn self_references(definition: &Value, own: Uuid) -> Vec<String> {
    let own = own.to_string();
    nodes(definition)
        .into_iter()
        .filter(|node| node.kind == "segment")
        .filter(|node| {
            node.value
                .get("segment")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq_ignore_ascii_case(&own))
        })
        .map(|node| format!("{}/segment", node.pointer))
        .collect()
}

/// The pure part of the tool: the arguments against the schema, then the checks a save applies,
/// against `catalog` at `now`. It returns the definition in its canonical wire form, or the
/// feedback lines. The golden-prompt harness runs it without a database.
pub(crate) fn check_draft(
    arguments: &Value,
    catalog: &Catalog,
    own: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Value, Vec<String>> {
    let schema = DraftSegment::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let args: DraftSegmentArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    check_definition(&args.definition, catalog, own, now)
        .map(|parsed| parsed.to_value())
        .map_err(|issues| lines_of(&issues))
}

/// The checks a save applies to a definition, against `catalog` at `now`: the parse and the
/// validation, that the segment `own` does not refer to itself, and the compiler (which
/// normalizes). Accepting a stored draft runs the same function.
pub(crate) fn check_definition(
    draft: &Value,
    catalog: &Catalog,
    own: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Definition, Vec<Issue>> {
    let parsed = parse_and_validate(draft, catalog)?;
    if let Some(own) = own {
        let cycles = self_references(draft, own);
        if !cycles.is_empty() {
            return Err(cycles
                .into_iter()
                .map(|pointer| Issue::new(pointer, "segment_cycle"))
                .collect());
        }
    }
    compile(&parsed, catalog, &TraitStorage::default(), now)?;
    Ok(parsed)
}

impl DraftSegment {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<DraftSegmentArgs>()
    }
}

impl AiTool<ToolContext> for DraftSegment {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "draft_segment"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Draft
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Proposes the definition of a segment as the `definition` argument. The \
                 definition is checked with the rules a save applies; the problems come back \
                 with their JSON pointers if it is refused. Nothing is saved."
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
            authorize_again(context).await?;
            let DraftTarget::Segment { segment_id } = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            let failed = |error: zirv_flow_store::StoreError| ToolError::Failed(error.into());
            let (snapshot, now) =
                zirv_flow_store::read_segment_catalog(&context.state.pool, context.workspace)
                    .await
                    .map_err(failed)?;
            let catalog = catalog_of(snapshot);
            check_draft(arguments, &catalog, *segment_id, now)
                .map(ToolOutput::Draft)
                .map_err(ToolError::Invalid)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn catalog(list: Uuid, segment: Uuid) -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("city".into(), TraitType::String);
        catalog.lists.insert(list);
        catalog.segments.insert(segment);
        catalog
    }

    const NOW: OffsetDateTime = datetime!(2026-05-15 12:00 UTC);

    #[test]
    fn the_parameters_hold_the_definition_as_an_object() {
        let tool = DraftSegment;
        assert_eq!(tool.name(), "draft_segment");
        assert_eq!(tool.effect(), ToolEffect::Draft);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(
            schema.as_value()["properties"]["definition"]["type"],
            "object"
        );
        let definition = json!({"v": 1, "trait": "plan", "op": "exists"});
        assert_eq!(
            schema.validate_value(&json!({"definition": definition})),
            Ok(())
        );
        assert!(
            schema
                .validate_value(&json!({"definition": "text"}))
                .is_err()
        );
    }

    #[test]
    fn a_valid_definition_comes_back_in_its_wire_form() {
        let (list, segment) = (Uuid::now_v7(), Uuid::now_v7());
        let arguments = json!({"definition": {"v": 1, "all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"not": {"list": list.to_string()}}
        ]}});

        let drafted = check_draft(&arguments, &catalog(list, segment), None, NOW).unwrap();

        assert_eq!(drafted, arguments["definition"]);
    }

    #[test]
    fn an_issue_is_a_code_and_a_pointer_and_never_a_value() {
        let (list, segment) = (Uuid::now_v7(), Uuid::now_v7());
        let arguments = json!({"definition": {"v": 1, "all": [
            {"trait": "ghost", "op": "eq", "value": "TOP-SECRET"},
            {"trait": "plan", "op": "gte", "value": "pro"},
            {"list": Uuid::now_v7().to_string()}
        ]}});

        let lines = check_draft(&arguments, &catalog(list, segment), None, NOW).unwrap_err();

        assert_eq!(
            lines,
            [
                "unknown_trait at /all/0/trait",
                "operator_not_allowed at /all/1/op",
                "unknown_list at /all/2/list"
            ]
        );
        assert!(!lines.join("").contains("TOP-SECRET"));
    }

    #[test]
    fn a_definition_that_does_not_parse_is_reported_by_the_parser() {
        let catalog = Catalog::default();

        let lines = check_draft(&json!({"definition": {}}), &catalog, None, NOW).unwrap_err();

        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| line.starts_with("malformed")));
    }

    #[test]
    fn arguments_that_are_not_a_definition_are_refused() {
        let catalog = Catalog::default();
        for arguments in [
            json!({}),
            json!({"definition": 3}),
            json!({"definition": {"v": 1}, "extra": 1}),
        ] {
            let lines = check_draft(&arguments, &catalog, None, NOW).unwrap_err();
            assert!(!lines.is_empty());
        }
    }

    #[test]
    fn a_segment_may_not_refer_to_itself() {
        let (list, segment) = (Uuid::now_v7(), Uuid::now_v7());
        let arguments = json!({"definition": {"v": 1, "all": [
            {"trait": "plan", "op": "exists"},
            {"not": {"segment": segment.to_string()}}
        ]}});

        let lines =
            check_draft(&arguments, &catalog(list, segment), Some(segment), NOW).unwrap_err();

        assert_eq!(lines, ["segment_cycle at /all/1/not/segment"]);
        // For a new segment, or another one, the reference is fine.
        assert!(check_draft(&arguments, &catalog(list, segment), None, NOW).is_ok());
        assert!(
            check_draft(
                &arguments,
                &catalog(list, segment),
                Some(Uuid::now_v7()),
                NOW
            )
            .is_ok()
        );
    }

    #[test]
    fn the_root_is_named_when_the_issue_is_on_the_whole_definition() {
        assert_eq!(
            line("unsupported_version", ""),
            "unsupported_version at the definition root"
        );
        assert_eq!(line("unknown_trait", "/trait"), "unknown_trait at /trait");
    }
}
