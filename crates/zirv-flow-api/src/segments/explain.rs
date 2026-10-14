//! Why one person is in or out of a segment (architecture 8.3): the reference evaluator run over
//! a bounded snapshot of the person, in the segment's version, with what each condition saw.
//! It runs on the primary, in one read-only snapshot under the preview's statement timeout.

use std::str::FromStr;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::segment::{AggFn, ConsentCondition};
use zirv_flow_segments::{
    Catalog, EvalNode, Node, Observed, Outcome, PersonSnapshot, SnapshotConsent, SnapshotEvent,
    evaluate, event_lower_bounds, parse_and_validate,
};
use zirv_flow_store::{
    ClearFields, ExplainEventScope, ExplainPerson, StoreError, find_segment, find_segment_version,
};

use super::{catalog_of, issues_error};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::problem::{ApiError, Problem};

/// How a node came out.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeBody {
    Pass,
    Fail,
    /// The person has more events of a name the node reads than one explain loads.
    Unknown,
}

impl From<Outcome> for OutcomeBody {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Pass => Self::Pass,
            Outcome::Fail => Self::Fail,
            Outcome::Unknown => Self::Unknown,
        }
    }
}

/// What a condition saw. Which fields are present depends on the node kind: a trait has `value`,
/// `masked` or `missing`; an event condition has `count` and, with an aggregate, `value` or
/// `masked`; a list, a segment, an identifier or consent has `present`, and consent also
/// `status`.
#[derive(Debug, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct ObservedBody {
    /// The stored trait value, or the aggregate over the matching events.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    /// True when the value is personal data and the caller may not see values.
    #[serde(skip_serializing_if = "Option::is_none")]
    masked: Option<bool>,
    /// True when the trait is not stored, or the key is not a property of the event.
    #[serde(skip_serializing_if = "Option::is_none")]
    missing: Option<bool>,
    /// How many events matched the condition's window and filters.
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    /// Whether the person holds the list, segment, identifier kind or consent state.
    #[serde(skip_serializing_if = "Option::is_none")]
    present: Option<bool>,
    /// The recorded consent status for the condition's channel and topic; `unknown` when there
    /// is no row.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ExplainNodeBody {
    /// The JSON pointer of the node in the segment version's definition; empty for the root.
    pointer: String,
    /// The node kind: `all`, `any`, `not`, `trait`, `event`, `list`, `segment`, `consent` or
    /// `identifier`.
    kind: String,
    outcome: OutcomeBody,
    /// What the condition saw; absent for `all`, `any` and `not`, and for an unknown node.
    #[serde(skip_serializing_if = "Option::is_none")]
    observed: Option<ObservedBody>,
    /// `capped` on an unknown node: the UI shows "unable to explain".
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[schema(no_recursion)]
    children: Vec<ExplainNodeBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExplainBody {
    segment_id: Uuid,
    /// The version evaluated: the current one unless `version` was asked for.
    version: i32,
    /// The person evaluated: the survivor when the id asked for was merged into another.
    person_id: Uuid,
    /// The id that was asked for, when it was merged into `person_id`; null otherwise.
    merged_from: Option<Uuid>,
    /// The time the evaluation ran at, which fixes every relative window.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    evaluated_at: OffsetDateTime,
    /// The fresh evaluation.
    outcome: OutcomeBody,
    /// Whether the person is an open member now, as stored. It can differ from `outcome` while a
    /// refresh is pending; the two are not reconciled.
    member: bool,
    /// When the open membership started; null when `member` is false.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    entered_at: Option<OffsetDateTime>,
    tree: ExplainNodeBody,
}

/// A read past its time limit is `explain_timeout`, never a 500.
fn read_error(error: StoreError) -> ApiError {
    if error.is_timeout() {
        ApiError::explain_timeout()
    } else {
        error.into()
    }
}

/// The evaluator's snapshot of what the store loaded.
fn snapshot_of(person: &ExplainPerson) -> PersonSnapshot {
    PersonSnapshot {
        traits: person.traits.clone(),
        events: person
            .events
            .iter()
            .map(|event| SnapshotEvent {
                name: event.name.clone(),
                occurred_at: event.occurred_at,
                properties: event.properties.clone(),
            })
            .collect(),
        capped_events: person.capped_events.clone(),
        lists: person.lists.clone(),
        consent: person
            .consent
            .iter()
            .map(|row| SnapshotConsent {
                channel: row.channel,
                topic: row.topic,
                status: row.status,
            })
            .collect(),
        identifier_kinds: person.identifier_kinds.clone(),
        segments: person.open_segments.keys().copied().collect(),
    }
}

/// What masking needs while the tree is built.
struct Masking<'a> {
    reveal: bool,
    clear: &'a ClearFields,
    catalog: &'a Catalog,
    person: &'a ExplainPerson,
}

impl Masking<'_> {
    fn hidden_trait(&self, key: &str) -> bool {
        !self.reveal && self.clear.is_trait_pii(key)
    }

    fn hidden_property(&self, event: &str, property: Option<&str>) -> bool {
        !self.reveal && property.is_none_or(|property| self.clear.is_property_pii(event, property))
    }

    /// The status recorded for a consent condition's channel and topic.
    fn consent_status(&self, condition: &ConsentCondition) -> String {
        let unknown = || "unknown".to_owned();
        let Ok(channel) = Channel::from_str(&condition.channel) else {
            return unknown();
        };
        let topic = match &condition.topic {
            None => None,
            Some(key) => {
                match self
                    .catalog
                    .topics
                    .get(&channel)
                    .and_then(|topics| topics.get(key))
                {
                    Some(id) => Some(*id),
                    None => return unknown(),
                }
            }
        };
        self.person
            .consent
            .iter()
            .find(|row| row.channel == channel && row.topic == topic)
            .map_or_else(unknown, |row| row.status.to_string())
    }

    fn observed(&self, node: &Node, seen: &Observed) -> ObservedBody {
        let mut body = ObservedBody::default();
        match (node, seen) {
            (_, Observed::Missing) => body.missing = Some(true),
            (Node::Trait(condition), Observed::Stored(value)) => {
                if self.hidden_trait(&condition.key) {
                    body.masked = Some(true);
                } else {
                    body.value = Some(value.clone());
                }
            }
            (_, Observed::Stored(_)) => {}
            (_, Observed::Matching { events }) => body.count = Some(*events),
            (Node::Event(condition), Observed::Aggregate { value, events }) => {
                body.count = Some(*events);
                // `count`, `first_time` and `last_time` read no property value.
                let personal = condition.agg.as_ref().is_some_and(|agg| {
                    matches!(agg.func, AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::Avg)
                        && self.hidden_property(&condition.name, agg.prop.as_deref())
                });
                if personal {
                    body.masked = Some(true);
                } else {
                    body.value.clone_from(value);
                }
            }
            (_, Observed::Aggregate { .. }) => {}
            (Node::Consent(condition), Observed::Present(held)) => {
                body.present = Some(*held);
                body.status = Some(self.consent_status(condition));
            }
            (_, Observed::Present(held)) => body.present = Some(*held),
        }
        body
    }

    fn node(&self, node: &Node, result: &EvalNode) -> ExplainNodeBody {
        let children: Vec<ExplainNodeBody> = match node {
            Node::All(items) | Node::Any(items) => items
                .iter()
                .zip(&result.children)
                .map(|(item, child)| self.node(item, child))
                .collect(),
            Node::Not(item) => result
                .children
                .first()
                .map(|child| self.node(item, child))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        let unknown = result.outcome == Outcome::Unknown;
        ExplainNodeBody {
            pointer: result.pointer.clone(),
            kind: node.key().to_owned(),
            outcome: result.outcome.into(),
            observed: result
                .observed
                .as_ref()
                .filter(|_| !unknown)
                .map(|seen| self.observed(node, seen)),
            reason: unknown.then(|| "capped".to_owned()),
            children,
        }
    }
}

/// Parses `segment` (required) and `version` (optional) from the query.
fn parse_query(query: &str) -> Result<(Uuid, Option<i32>), ApiError> {
    let mut segment = None;
    let mut version = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "segment" => {
                segment = Some(
                    Uuid::parse_str(&value)
                        .map_err(|_| ApiError::bad_request("segment is not a valid id"))?,
                );
            }
            "version" => {
                version = Some(
                    value
                        .parse::<i32>()
                        .ok()
                        .filter(|number| *number > 0)
                        .ok_or_else(|| {
                            ApiError::bad_request("version must be a positive whole number")
                        })?,
                );
            }
            _ => {}
        }
    }
    let segment = segment.ok_or_else(|| ApiError::bad_request("segment is required"))?;
    Ok((segment, version))
}

/// Why one person is in or out of a segment. The reference evaluator runs the segment's current
/// version (or `version`) over the person's traits, the events the definition reads, lists,
/// consent, identifier kinds and open memberships, and returns the tree of conditions with what
/// each saw. `member` and `entered_at` are the stored membership, which can differ from the fresh
/// `outcome` while a refresh is pending. A person with more events of a name than one explain
/// loads makes the conditions on that name `unknown` with the reason `capped`. A person that was
/// merged into another is explained as the survivor, with `merged_from` set. PII trait values and
/// PII event-property values and aggregates are masked unless the caller is the owner, a
/// workspace admin or an `admin` key; identifier values are never shown. A saved version that
/// no longer validates against the workspace is 422 `segment_invalid`. A read that runs past its
/// time limit is 503 with the code `explain_timeout`.
#[utoipa::path(
    get,
    operation_id = "explain_person_segment",
    path = "/api/v1/workspaces/{ws}/people/{id}/explain",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("segment" = Uuid, Query, description = "The segment to explain"),
        ("version" = Option<i32>, Query, description = "The segment version; default the current one")
    ),
    responses(
        (status = 200, description = "The evaluation tree", body = ExplainBody),
        (status = 400, description = "segment is missing or malformed, or version is not a positive whole number", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, person, segment or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The saved version no longer validates (`code` is `segment_invalid`, with `issues`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `explain_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn explain(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<ExplainBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let requested: Uuid = path_id(&id)?;
    let (segment_id, asked) = parse_query(query.as_deref().unwrap_or_default())?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;

    let mut tx =
        zirv_flow_store::begin_segment_snapshot(&state.pool, state.segment_preview_timeout)
            .await
            .map_err(read_error)?;
    let now = state.clock.now(&mut tx).await.map_err(read_error)?;
    let segment = find_segment(&mut *tx, workspace.id, segment_id)
        .await
        .map_err(read_error)?
        .ok_or_else(ApiError::not_found)?;
    let version = asked.unwrap_or(segment.current_version);
    let saved = find_segment_version(&mut *tx, workspace.id, segment_id, version)
        .await
        .map_err(read_error)?
        .ok_or_else(ApiError::not_found)?;
    let catalog = catalog_of(
        zirv_flow_store::segment_catalog(&mut tx, workspace.id)
            .await
            .map_err(read_error)?,
    );
    let definition = parse_and_validate(&saved.definition, &catalog).map_err(issues_error)?;
    let scope: Vec<ExplainEventScope> = event_lower_bounds(&definition, now)
        .into_iter()
        .map(|(name, since)| ExplainEventScope { name, since })
        .collect();
    let person = zirv_flow_store::load_explain_person(&mut tx, workspace.id, requested, &scope)
        .await
        .map_err(read_error)?
        .ok_or_else(ApiError::not_found)?;
    let clear = zirv_flow_store::explain_clear_fields(&mut tx, workspace.id)
        .await
        .map_err(read_error)?;
    drop(tx);

    let evaluation = evaluate(&definition, &catalog, &snapshot_of(&person), now);
    let masking = Masking {
        reveal,
        clear: &clear,
        catalog: &catalog,
        person: &person,
    };
    let entered_at = person.open_segments.get(&segment_id).copied();
    Ok(Json(ExplainBody {
        segment_id,
        version,
        person_id: person.id,
        merged_from: person.merged_from,
        evaluated_at: now,
        outcome: evaluation.outcome.into(),
        member: entered_at.is_some(),
        entered_at,
        tree: masking.node(&definition.root, &evaluation.tree),
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use axum::http::StatusCode;
    use hashbrown::HashMap;
    use serde_json::json;
    use zirv_flow_core::consent::ConsentStatus;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_segments::Definition;
    use zirv_flow_store::{ExplainConsent, ExplainEvent};

    use super::*;

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;
    const LIST: &str = "018f0000-0000-7000-8000-000000000001";

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("city".into(), TraitType::String);
        let mut order = HashMap::new();
        order.insert("total".to_owned(), PropertyType::Number);
        order.insert("sku".to_owned(), PropertyType::String);
        catalog.events.insert("order".to_owned(), order);
        catalog.lists.insert(Uuid::parse_str(LIST).unwrap());
        catalog
    }

    fn clear() -> ClearFields {
        ClearFields::new(["plan"], [("order", "sku")])
    }

    fn person() -> ExplainPerson {
        ExplainPerson {
            id: Uuid::nil(),
            merged_from: None,
            traits: json!({"plan": "pro", "city": "Aarhus"})
                .as_object()
                .cloned()
                .unwrap(),
            events: ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
                .into_iter()
                .map(|at| ExplainEvent {
                    name: "order".into(),
                    occurred_at: OffsetDateTime::parse(
                        at,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .unwrap(),
                    properties: json!({"total": 30.5, "sku": "a"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                })
                .collect(),
            capped_events: BTreeSet::new(),
            lists: BTreeSet::new(),
            consent: vec![ExplainConsent {
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Unsubscribed,
            }],
            identifier_kinds: BTreeSet::from(["email".to_owned()]),
            open_segments: BTreeMap::new(),
        }
    }

    /// Builds the tree of `root` over `person`, as the handler does.
    fn tree(root: Value, person: &ExplainPerson, reveal: bool) -> ExplainNodeBody {
        let mut wire = root;
        wire["v"] = json!(1);
        let definition = Definition::from_value(&wire).unwrap();
        let catalog = catalog();
        let evaluation = evaluate(&definition, &catalog, &snapshot_of(person), NOW);
        let clear = clear();
        Masking {
            reveal,
            clear: &clear,
            catalog: &catalog,
            person,
        }
        .node(&definition.root, &evaluation.tree)
    }

    fn json_of(body: &ExplainNodeBody) -> Value {
        serde_json::to_value(body).unwrap()
    }

    #[test]
    fn a_pii_trait_is_masked_and_a_clear_one_is_shown() {
        let root = json!({"all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"trait": "city", "op": "eq", "value": "Aarhus"},
        ]});

        let hidden = json_of(&tree(root.clone(), &person(), false));
        let shown = json_of(&tree(root, &person(), true));

        assert_eq!(hidden["children"][0]["observed"], json!({"value": "pro"}));
        assert_eq!(hidden["children"][1]["observed"], json!({"masked": true}));
        assert_eq!(shown["children"][1]["observed"], json!({"value": "Aarhus"}));
    }

    #[test]
    fn a_missing_trait_is_missing_not_masked() {
        let mut nobody = person();
        nobody.traits.clear();

        let body = json_of(&tree(
            json!({"trait": "city", "op": "exists"}),
            &nobody,
            false,
        ));

        assert_eq!(body["outcome"], "fail");
        assert_eq!(body["observed"], json!({"missing": true}));
    }

    #[test]
    fn an_event_reports_its_count_and_masks_an_aggregate_over_a_pii_property() {
        let sum = |prop: &str| {
            json!({"event": "order",
                "agg": {"fn": "sum", "prop": prop, "op": "gte", "value": 50}})
        };

        let pii = json_of(&tree(sum("total"), &person(), false));
        let revealed = json_of(&tree(sum("total"), &person(), true));

        assert_eq!(pii["kind"], "event");
        assert_eq!(pii["observed"], json!({"count": 2, "masked": true}));
        assert_eq!(revealed["observed"], json!({"count": 2, "value": 61.0}));
    }

    #[test]
    fn a_clear_property_aggregate_and_the_count_and_times_are_never_masked() {
        let count = json!({"event": "order",
            "agg": {"fn": "count", "op": "gte", "value": 2}});
        let last = json!({"event": "order",
            "agg": {"fn": "last_time", "op": "after", "value": "2025-01-01T00:00:00Z"}});

        let count = json_of(&tree(count, &person(), false));
        let last = json_of(&tree(last, &person(), false));

        assert_eq!(count["outcome"], "pass");
        assert_eq!(count["observed"], json!({"count": 2, "value": 2}));
        assert_eq!(last["observed"]["value"], "2026-01-02T00:00:00Z");
        assert!(last["observed"].get("masked").is_none());
    }

    #[test]
    fn an_event_without_an_aggregate_reports_how_many_matched() {
        let body = json_of(&tree(json!({"event": "order"}), &person(), false));

        assert_eq!(body["observed"], json!({"count": 2}));
    }

    #[test]
    fn lists_and_identifiers_report_presence_and_never_a_value() {
        let root = json!({"all": [
            {"list": LIST},
            {"identifier": "email"},
            {"consent": {"channel": "email", "status": "subscribed"}},
        ]});

        let body = json_of(&tree(root, &person(), true));

        assert_eq!(body["children"][0]["observed"], json!({"present": false}));
        assert_eq!(body["children"][1]["observed"], json!({"present": true}));
        assert_eq!(
            body["children"][2]["observed"],
            json!({"present": false, "status": "unsubscribed"})
        );
        assert!(!body.to_string().contains("ada@example.com"));
    }

    #[test]
    fn a_capped_event_is_unknown_with_a_reason_and_unknown_spreads_up() {
        let mut capped = person();
        capped.capped_events.insert("order".to_owned());
        let root = json!({"all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"not": {"event": "order"}},
        ]});

        let body = json_of(&tree(root, &capped, false));

        assert_eq!(body["outcome"], "unknown");
        assert_eq!(body["reason"], "capped");
        assert_eq!(body["children"][0]["outcome"], "pass");
        assert!(body["children"][0].get("reason").is_none());
        assert_eq!(body["children"][1]["outcome"], "unknown");
        assert_eq!(body["children"][1]["reason"], "capped");
        let leaf = &body["children"][1]["children"][0];
        assert_eq!(leaf["pointer"], "/all/1/not");
        assert_eq!(leaf["outcome"], "unknown");
        assert!(leaf.get("observed").is_none());
    }

    #[test]
    fn a_known_failing_child_decides_over_an_unknown_one() {
        let mut capped = person();
        capped.capped_events.insert("order".to_owned());
        let root = json!({"all": [
            {"trait": "plan", "op": "eq", "value": "free"},
            {"event": "order"},
        ]});

        let body = json_of(&tree(root, &capped, false));

        assert_eq!(body["outcome"], "fail");
        assert!(body.get("reason").is_none());
    }

    #[test]
    fn the_query_needs_a_segment_and_reads_a_version() {
        let id = Uuid::now_v7();

        assert_eq!(parse_query(&format!("segment={id}")).unwrap(), (id, None));
        assert_eq!(
            parse_query(&format!("segment={id}&version=3")).unwrap(),
            (id, Some(3))
        );
        for bad in [
            String::new(),
            "segment=nope".to_owned(),
            format!("segment={id}&version=0"),
            format!("segment={id}&version=x"),
        ] {
            assert_eq!(
                parse_query(&bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_timeout_is_a_503_with_its_own_code() {
        let error = ApiError::explain_timeout();

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), Some("explain_timeout"));
    }
}
