//! The render inputs of a block email at send time (architecture 11.4): the theme in effect, the
//! current snippet versions, asset URLs and the results of the `visible_if` conditions. A send
//! and a test send resolve them with the same loader and conversion as the preview in `api`, so
//! what was previewed is what is sent. The versions used come back to be recorded.

use serde_json::{Map, Value};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_segments::{Outcome, PersonSnapshot, compile, evaluate, parse_condition};
use zirv_flow_store::{
    CompiledCondition, ExtrasRequest, ThemeRequest, evaluate_conditions, load_render_inputs,
    segment_catalog,
};
use zirv_flow_templates::{
    Loaded, RenderContext, RenderExtras, TemplateDocument, ThemeNeed, extras_needs, render_extras,
};

use super::render::RENDER_ERROR;
use crate::JobError;
use crate::kinds::segment_refresh::{catalog_of, sql_query};

/// Whose values the conditions are read from.
pub(crate) enum Subject<'a> {
    /// A real person, by the SQL evaluator.
    Person(Uuid),
    /// A sample person, who is in no segment, by the reference evaluator.
    Sample(&'a RenderContext),
}

/// What a message was rendered with, to record.
#[derive(Debug)]
pub(crate) struct Versions {
    /// The theme in effect and its version.
    pub(crate) theme: Option<(Uuid, i32)>,
    /// Snippet id to the version rendered, as an object.
    pub(crate) snippet_versions: Value,
}

/// The extras and what to record about them.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) extras: RenderExtras,
    pub(crate) versions: Versions,
}

fn request_of(doc: &TemplateDocument) -> ExtrasRequest {
    let needs = extras_needs(doc);
    ExtrasRequest {
        theme: match needs.theme {
            ThemeNeed::None => ThemeRequest::None,
            ThemeNeed::Default => ThemeRequest::Default,
            ThemeNeed::Id(id) => ThemeRequest::Id(id),
        },
        footer: needs.footer,
        snippets: needs.snippets,
        assets: needs.assets,
    }
}

/// Resolves the inputs of `doc`. `Ok(Err(reason))` is a document that cannot be rendered: a
/// condition that no longer compiles. A theme, snippet or asset that is gone is not an error here;
/// the render reports it as a broken reference. `public_url` is `server.public_url`; without it
/// the asset URLs are relative and a render that names an asset fails.
pub(crate) async fn resolve(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    doc: &TemplateDocument,
    subject: Subject<'_>,
    public_url: Option<&str>,
    now: OffsetDateTime,
) -> Result<Result<Resolved, &'static str>, JobError> {
    let loaded = load_render_inputs(connection, workspace, &request_of(doc), public_url).await?;
    let needs = extras_needs(doc);
    let mut results = std::collections::BTreeMap::new();
    if !needs.conditions.is_empty() {
        let (catalog, storage) = catalog_of(segment_catalog(connection, workspace).await?);
        let mut parsed = Vec::with_capacity(needs.conditions.len());
        for (id, condition) in &needs.conditions {
            let Ok(definition) = parse_condition(condition, &catalog) else {
                return Ok(Err(RENDER_ERROR));
            };
            let Ok(compiled) = compile(&definition, &catalog, &storage, now) else {
                return Ok(Err(RENDER_ERROR));
            };
            parsed.push((id, definition, compiled));
        }
        let shown: Vec<bool> = match subject {
            Subject::Person(person) => {
                let queries: Vec<CompiledCondition> = parsed
                    .iter()
                    .map(|(_, _, compiled)| sql_query(compiled.batch_sql()))
                    .collect();
                evaluate_conditions(connection, workspace, person, &queries).await?
            }
            Subject::Sample(context) => {
                let snapshot = sample_snapshot(context);
                parsed
                    .iter()
                    .map(|(_, definition, _)| {
                        evaluate(definition, &catalog, &snapshot, now).outcome == Outcome::Pass
                    })
                    .collect()
            }
        };
        results.extend(
            parsed
                .iter()
                .zip(shown)
                .map(|((id, ..), shown)| ((*id).clone(), shown)),
        );
    }
    let extras = render_extras(Loaded {
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
    let snippet_versions = loaded
        .snippets
        .iter()
        .map(|(id, snippet)| (id.clone(), Value::from(snippet.version)))
        .collect::<Map<_, _>>();
    Ok(Ok(Resolved {
        extras,
        versions: Versions {
            theme: loaded.theme.map(|theme| (theme.theme_id, theme.version)),
            snippet_versions: Value::Object(snippet_versions),
        },
    }))
}

/// The snapshot of a sample person: the traits of the sample context and nothing else.
fn sample_snapshot(context: &RenderContext) -> PersonSnapshot {
    PersonSnapshot {
        traits: context
            .person
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        ..PersonSnapshot::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_templates::parse_template_document;

    use super::*;

    #[test]
    fn a_sample_person_has_the_sample_traits_and_nothing_else() {
        let mut context = RenderContext::default();
        context.person.insert("plan".into(), json!("sample"));

        let snapshot = sample_snapshot(&context);

        assert_eq!(snapshot.traits["plan"], json!("sample"));
        assert!(snapshot.lists.is_empty() && snapshot.segments.is_empty());
        assert!(snapshot.events.is_empty());
    }

    #[test]
    fn the_document_chooses_the_theme_the_store_loads() {
        let doc = |theme: Value| {
            parse_template_document(&json!({
                "schema": 2, "channel": "email", "mode": "blocks",
                "blocks": {"subject": "Hi", "category": "transactional", "theme": theme,
                           "sections": []}
            }))
            .unwrap()
        };

        assert_eq!(request_of(&doc(json!("none"))).theme, ThemeRequest::None);
        assert_eq!(
            request_of(&doc(json!("default"))).theme,
            ThemeRequest::Default
        );
        assert_eq!(
            request_of(&doc(json!({"id": "abc"}))).theme,
            ThemeRequest::Id("abc".into())
        );
    }
}
