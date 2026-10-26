//! The render inputs of a block email for preview and publish (architecture 11.4): the theme in
//! effect, the current snippet versions, asset URLs and the results of the `visible_if`
//! conditions. The loading is the store's `load_render_inputs` and the conversion is
//! `zirv_flow_templates::render_extras`, the pair a send calls too, so a preview renders what a
//! send renders.

use std::collections::BTreeMap;

use serde_json::Map;

use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_segments::{
    Catalog, ConditionError, Definition, EvalNode, Node, Outcome, PersonSnapshot, compile,
    evaluate, event_lower_bounds, parse_condition,
};
use zirv_flow_store::{
    CompiledCondition, ExplainEventScope, ExtrasRequest, LoadedInputs, PgConnection, ThemeRequest,
    evaluate_conditions, load_explain_person, load_render_inputs, segment_catalog,
};
use zirv_flow_templates::{
    ExtrasNeeds, Issue, Loaded, RenderContext, RenderExtras, TemplateDocument, ThemeNeed, code,
    extras_needs, render_extras,
};

use crate::problem::ApiError;
use crate::segments::catalog_of;
use crate::segments::explain::snapshot_of;
use crate::segments::preview::sql_query;
use crate::snippets::preview::INERT_ASSET_HOST;

/// Who the conditions are evaluated for.
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    /// No one: the conditions are checked but not evaluated (a publish).
    Nobody,
    /// A real person.
    Person(Uuid),
    /// A sample person, who is in no segment.
    Sample(&'a RenderContext),
}

/// The resolved inputs of one document.
pub(crate) struct Inputs {
    pub(crate) extras: RenderExtras,
    /// What was loaded, for the message record and the asset references.
    pub(crate) loaded: LoadedInputs,
    /// The conditions that are not usable, as blocking issues.
    pub(crate) issues: Vec<Issue>,
    /// Why each condition that was false is, by the id of its section or block.
    pub(crate) reasons: BTreeMap<String, String>,
}

/// The base asset URLs are built on in a preview or a publish check: the public URL when it is
/// `https`, else a host that never resolves, because the renderer takes `https` URLs only.
pub(crate) fn asset_base(public_url: Option<&str>) -> &str {
    public_url
        .filter(|url| url.starts_with("https://"))
        .unwrap_or(INERT_ASSET_HOST)
}

/// The store's request for what a document names.
pub(crate) fn request_of(needs: &ExtrasNeeds) -> ExtrasRequest {
    ExtrasRequest {
        theme: match &needs.theme {
            ThemeNeed::None => ThemeRequest::None,
            ThemeNeed::Default => ThemeRequest::Default,
            ThemeNeed::Id(id) => ThemeRequest::Id(id.clone()),
        },
        footer: needs.footer,
        snippets: needs.snippets.clone(),
        assets: needs.assets.clone(),
    }
}

/// Loads the inputs of `doc` and evaluates its conditions for `subject`. A document with nothing
/// to resolve gets empty extras without a query.
pub(crate) async fn resolve(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    doc: &TemplateDocument,
    subject: Subject<'_>,
    public_url: Option<&str>,
    now: OffsetDateTime,
) -> Result<Inputs, ApiError> {
    let needs = extras_needs(doc);
    let loaded = load_render_inputs(
        connection,
        workspace,
        &request_of(&needs),
        Some(asset_base(public_url)),
    )
    .await?;
    let mut inputs = Inputs {
        extras: RenderExtras::default(),
        loaded,
        issues: Vec::new(),
        reasons: BTreeMap::new(),
    };
    let results = if needs.conditions.is_empty() {
        BTreeMap::new()
    } else {
        conditions(
            connection,
            workspace,
            doc,
            &needs,
            subject,
            now,
            &mut inputs,
        )
        .await?
    };
    let loaded = &inputs.loaded;
    inputs.extras = render_extras(Loaded {
        theme: loaded
            .theme
            .as_ref()
            .map(|theme| (theme.name.as_str(), &theme.tokens)),
        snippets: loaded
            .snippets
            .iter()
            .map(|(id, snippet)| (id.as_str(), &snippet.content))
            .collect(),
        assets: loaded.assets.clone(),
        conditions: results,
    });
    Ok(inputs)
}

/// The issue for a condition that cannot be used, at the path of the block or section.
pub(crate) fn condition_issue(doc: &TemplateDocument, id: &str, error: &ConditionError) -> Issue {
    let is_section = doc
        .blocks
        .as_ref()
        .is_some_and(|blocks| blocks.sections.iter().any(|section| section.id == id));
    let at = format!(
        "/blocks/{}/{id}/visible_if",
        if is_section { "sections" } else { "block" }
    );
    let kind = if error.is_broken_segment() {
        code::BROKEN_SEGMENT_REFERENCE
    } else {
        code::INVALID_CONDITION
    };
    let codes: Vec<&str> = error.issues.iter().map(|issue| issue.code).collect();
    Issue::new(kind, at).with_message(codes.join(", "))
}

async fn conditions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    doc: &TemplateDocument,
    needs: &ExtrasNeeds,
    subject: Subject<'_>,
    now: OffsetDateTime,
    inputs: &mut Inputs,
) -> Result<BTreeMap<String, bool>, ApiError> {
    let (catalog, storage) = catalog_of(segment_catalog(connection, workspace).await?);
    let mut usable: Vec<(&str, Definition, CompiledCondition)> = Vec::new();
    for (id, condition) in &needs.conditions {
        let compiled = parse_condition(condition, &catalog).and_then(|definition| {
            compile(&definition, &catalog, &storage, now)
                .map(|compiled| (definition, compiled))
                .map_err(|issues| ConditionError { issues })
        });
        match compiled {
            Ok((definition, compiled)) => {
                usable.push((id, definition, sql_query(compiled.batch_sql())));
            }
            Err(error) => inputs.issues.push(condition_issue(doc, id, &error)),
        }
    }
    let results = match subject {
        Subject::Nobody => return Ok(BTreeMap::new()),
        Subject::Person(person) => {
            let queries: Vec<CompiledCondition> =
                usable.iter().map(|(_, _, query)| query.clone()).collect();
            evaluate_conditions(connection, workspace, person, &queries).await?
        }
        Subject::Sample(context) => {
            let snapshot = sample_snapshot(context);
            usable
                .iter()
                .map(|(_, definition, _)| {
                    evaluate(definition, &catalog, &snapshot, now).outcome == Outcome::Pass
                })
                .collect()
        }
    };
    let mut by_id = BTreeMap::new();
    for ((id, definition, _), shown) in usable.iter().zip(results) {
        by_id.insert((*id).to_owned(), shown);
        if !shown {
            let reason = match subject {
                Subject::Person(person) => {
                    person_reason(connection, workspace, person, definition, &catalog, now).await?
                }
                Subject::Sample(context) => {
                    sample_reason(definition, &catalog, sample_snapshot(context), now)
                }
                Subject::Nobody => continue,
            };
            inputs.reasons.insert((*id).to_owned(), reason);
        }
    }
    Ok(by_id)
}

/// The snapshot of a sample person: the traits of the sample context, no event, list, consent or
/// segment.
fn sample_snapshot(context: &RenderContext) -> PersonSnapshot {
    PersonSnapshot {
        traits: context
            .person
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<_, _>>(),
        ..PersonSnapshot::default()
    }
}

fn sample_reason(
    definition: &Definition,
    catalog: &Catalog,
    snapshot: PersonSnapshot,
    now: OffsetDateTime,
) -> String {
    let evaluation = evaluate(definition, catalog, &snapshot, now);
    unmet(&definition.root, &evaluation.tree)
}

/// Why a real person does not meet the condition: the reference evaluator over the person's
/// traits and the events the condition reads, as the explain route does.
async fn person_reason(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    definition: &Definition,
    catalog: &Catalog,
    now: OffsetDateTime,
) -> Result<String, ApiError> {
    let scope: Vec<ExplainEventScope> = event_lower_bounds(definition, now)
        .into_iter()
        .map(|(name, since)| ExplainEventScope { name, since })
        .collect();
    let Some(found) = load_explain_person(connection, workspace, person, &scope).await? else {
        return Ok("the condition was not met".to_owned());
    };
    let evaluation = evaluate(definition, catalog, &snapshot_of(&found), now);
    Ok(unmet(&definition.root, &evaluation.tree))
}

/// Names the first condition of the tree that was not met. Only names appear, never a value.
fn unmet(node: &Node, result: &EvalNode) -> String {
    let outcome = match result.outcome {
        Outcome::Unknown => "could not be decided",
        _ => "was not met",
    };
    match node {
        Node::All(items) => items
            .iter()
            .zip(&result.children)
            .find(|(_, child)| child.outcome != Outcome::Pass)
            .map_or_else(
                || format!("`all` {outcome}"),
                |(item, child)| unmet(item, child),
            ),
        Node::Any(items) => format!("none of the {} conditions of `any` was met", items.len()),
        Node::Not(item) => format!("the condition under `not` was met: {}", describe(item)),
        leaf => format!("{} {outcome}", describe(leaf)),
    }
}

fn describe(node: &Node) -> String {
    match node {
        Node::Trait(condition) => format!("trait `{}`", condition.key),
        Node::Event(condition) => format!("event `{}`", condition.name),
        Node::Identifier(kind) => format!("identifier `{kind}`"),
        other => format!("`{}`", other.key()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_templates::parse_template_document;

    use super::*;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("city".into(), TraitType::String);
        catalog
    }

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn reason(condition: serde_json::Value, traits: serde_json::Value) -> String {
        let definition = parse_condition(&condition, &catalog()).unwrap();
        let snapshot = PersonSnapshot {
            traits: traits.as_object().cloned().unwrap(),
            ..PersonSnapshot::default()
        };
        sample_reason(&definition, &catalog(), snapshot, NOW)
    }

    #[test]
    fn the_reason_names_the_first_unmet_leaf_and_never_a_value() {
        let all = json!({"all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"trait": "city", "op": "eq", "value": "Aarhus"}
        ]});

        let text = reason(all.clone(), json!({"plan": "pro", "city": "Oslo"}));

        assert_eq!(text, "trait `city` was not met");
        assert!(!text.contains("Oslo") && !text.contains("Aarhus"));
        assert_eq!(
            reason(all, json!({"plan": "free", "city": "Oslo"})),
            "trait `plan` was not met"
        );
    }

    #[test]
    fn any_and_not_name_their_kind() {
        let any = json!({"any": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"trait": "city", "op": "eq", "value": "Aarhus"}
        ]});
        let not = json!({"not": {"trait": "plan", "op": "eq", "value": "pro"}});

        assert_eq!(
            reason(any, json!({})),
            "none of the 2 conditions of `any` was met"
        );
        assert_eq!(
            reason(not, json!({"plan": "pro"})),
            "the condition under `not` was met: trait `plan`"
        );
    }

    #[test]
    fn a_condition_issue_is_at_the_block_or_the_section_and_keeps_its_kind() {
        let doc = parse_template_document(&json!({
            "schema": 2, "channel": "email", "mode": "blocks",
            "blocks": {"subject": "Hi", "category": "transactional", "sections": [
                {"id": "s1", "columns": [[{"type": "divider", "id": "b1"}]]}
            ]}
        }))
        .unwrap();
        let invalid = parse_condition(
            &json!({"trait": "nope", "op": "eq", "value": "x"}),
            &catalog(),
        )
        .unwrap_err();
        let broken = parse_condition(&json!({"segment": Uuid::now_v7().to_string()}), &catalog())
            .unwrap_err();

        let on_block = condition_issue(&doc, "b1", &invalid);
        let on_section = condition_issue(&doc, "s1", &broken);

        assert_eq!(
            (on_block.code, on_block.at.as_str()),
            ("invalid_condition", "/blocks/block/b1/visible_if")
        );
        assert_eq!(
            (on_section.code, on_section.at.as_str()),
            ("broken_segment_reference", "/blocks/sections/s1/visible_if")
        );
    }

    #[test]
    fn asset_urls_use_the_public_url_only_when_it_is_https() {
        assert_eq!(
            asset_base(Some("https://flow.example.com")),
            "https://flow.example.com"
        );
        assert_eq!(asset_base(Some("http://localhost:8080")), INERT_ASSET_HOST);
        assert_eq!(asset_base(None), INERT_ASSET_HOST);
    }

    #[test]
    fn a_needs_theme_maps_to_the_stores_request() {
        let needs = ExtrasNeeds {
            theme: ThemeNeed::Id("t".into()),
            footer: true,
            snippets: vec!["s".into()],
            assets: vec!["a".into()],
            conditions: Vec::new(),
        };

        let request = request_of(&needs);

        assert_eq!(request.theme, ThemeRequest::Id("t".into()));
        assert_eq!(
            (request.snippets, request.assets),
            (vec!["s".into()], vec!["a".into()])
        );
    }
}
