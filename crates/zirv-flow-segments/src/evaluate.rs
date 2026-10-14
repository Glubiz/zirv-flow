//! The reference evaluator (architecture sections 8.1 and 8.2): the segment AST evaluated in
//! memory over one [`PersonSnapshot`]. It is the oracle the compiled SQL is compared with, and
//! the engine behind the explain view, so it returns the whole tree with what each leaf saw.
//!
//! It follows the SQL byte for byte in behaviour (`lower.rs`, and `notes.md` of #119): leaves
//! are two-valued, `not` is plain negation, a zero-event aggregate other than `count` is false,
//! and a stored value the SQL cannot read is missing. The only third value is unknown, and only
//! a cap makes it (ruling 0c6404db): a condition that reads an event name in
//! [`PersonSnapshot::capped_events`] cannot be decided from the events held. With no caps the
//! outcome is always pass or fail.
//!
//! `evaluate` takes a validated definition and does not normalize it, so the tree it returns
//! has the JSON pointers of the definition the user wrote.

use std::str::FromStr;

use serde_json::Value;
use time::OffsetDateTime;
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;
use zirv_flow_core::segment::{
    AggFn, ConsentCondition, Definition, EventCondition, Node, Window, pointer_push,
};

use crate::catalog::Catalog;
use crate::leaf::{absolute, leaf, number, span_of, stored_number, stored_time_of, time};
use crate::snapshot::{PersonSnapshot, SnapshotEvent};
use crate::stored_time;
use crate::validate::Kind;

/// How a node came out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Pass,
    Fail,
    Unknown,
}

impl Outcome {
    fn of(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Fail }
    }

    fn negated(self) -> Self {
        match self {
            Self::Pass => Self::Fail,
            Self::Fail => Self::Pass,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// What a leaf saw, for the explain view to render and mask.
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// The key is not stored (or is not a property of the event), or the node is unknown.
    Missing,
    /// A trait leaf: the stored value, whatever its type.
    Stored(Value),
    /// An event condition without an aggregate: how many events matched.
    Matching { events: usize },
    /// An event condition with an aggregate: its value over the matching events, or `None` when
    /// there was nothing to aggregate, and how many events matched.
    Aggregate { value: Option<Value>, events: usize },
    /// A list, segment, consent, or identifier condition: whether the person holds it.
    Present(bool),
}

/// One node of the result, at the pointer it has in the evaluated definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EvalNode {
    pub pointer: String,
    pub outcome: Outcome,
    pub observed: Option<Observed>,
    /// The children of `all`, `any` and `not`; a leaf has none.
    pub children: Vec<EvalNode>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub outcome: Outcome,
    pub tree: EvalNode,
}

/// Evaluates a validated definition for one person at the explicit time `now`.
pub fn evaluate(
    definition: &Definition,
    catalog: &Catalog,
    person: &PersonSnapshot,
    now: OffsetDateTime,
) -> Evaluation {
    let evaluator = Evaluator {
        catalog,
        person,
        now: stored_time::micros(now),
    };
    let tree = evaluator.node(&definition.root, String::new());
    Evaluation {
        outcome: tree.outcome,
        tree,
    }
}

struct Evaluator<'a> {
    catalog: &'a Catalog,
    person: &'a PersonSnapshot,
    /// Microseconds since the epoch.
    now: i64,
}

impl Evaluator<'_> {
    fn node(&self, node: &Node, pointer: String) -> EvalNode {
        match node {
            Node::All(children) => self.group(children, pointer, true),
            Node::Any(children) => self.group(children, pointer, false),
            Node::Not(child) => {
                let child = self.node(child, pointer_push(&pointer, "not"));
                EvalNode {
                    pointer,
                    outcome: child.outcome.negated(),
                    observed: None,
                    children: vec![child],
                }
            }
            Node::Trait(condition) => {
                let stored = self.person.traits.get(&condition.key);
                let kind = self
                    .catalog
                    .traits
                    .get(&condition.key)
                    .map(|kind| Kind::from(*kind));
                let passed = kind.is_some_and(|kind| {
                    leaf(
                        kind,
                        stored,
                        condition.op,
                        condition.value.as_ref(),
                        self.now,
                    )
                });
                let observed =
                    stored.map_or(Observed::Missing, |value| Observed::Stored(value.clone()));
                self.leaf(pointer, Outcome::of(passed), observed)
            }
            Node::Event(condition) => self.event(condition, pointer),
            Node::List(id) => self.held(pointer, self.person.lists.contains(id)),
            Node::Segment(id) => self.held(pointer, self.person.segments.contains(id)),
            Node::Consent(condition) => {
                let held = self.consent(condition);
                self.held(pointer, held)
            }
            Node::Identifier(kind) => {
                self.held(pointer, self.person.identifier_kinds.contains(kind))
            }
        }
    }

    fn leaf(&self, pointer: String, outcome: Outcome, observed: Observed) -> EvalNode {
        EvalNode {
            pointer,
            outcome,
            observed: Some(observed),
            children: Vec::new(),
        }
    }

    fn held(&self, pointer: String, held: bool) -> EvalNode {
        self.leaf(pointer, Outcome::of(held), Observed::Present(held))
    }

    /// Three-valued `all` and `any`: a known deciding child wins over an unknown one.
    fn group(&self, children: &[Node], pointer: String, all: bool) -> EvalNode {
        let base = pointer_push(&pointer, if all { "all" } else { "any" });
        let children: Vec<EvalNode> = children
            .iter()
            .enumerate()
            .map(|(index, child)| self.node(child, pointer_push(&base, &index.to_string())))
            .collect();
        let deciding = if all { Outcome::Fail } else { Outcome::Pass };
        let outcome = if children.iter().any(|child| child.outcome == deciding) {
            deciding
        } else if children
            .iter()
            .any(|child| child.outcome == Outcome::Unknown)
        {
            Outcome::Unknown
        } else {
            deciding.negated()
        };
        EvalNode {
            pointer,
            outcome,
            observed: None,
            children,
        }
    }

    /// A consent row for the channel and topic, or none. `unknown` is what no row means, and
    /// what a stored `unknown` row means too.
    fn consent(&self, condition: &ConsentCondition) -> bool {
        let (Ok(channel), Ok(status)) = (
            Channel::from_str(&condition.channel),
            ConsentStatus::from_str(&condition.status),
        ) else {
            return false;
        };
        let topic = match &condition.topic {
            None => None,
            Some(key) => {
                let id = self
                    .catalog
                    .topics
                    .get(&channel)
                    .and_then(|topics| topics.get(key));
                match id {
                    Some(id) => Some(*id),
                    None => return false,
                }
            }
        };
        let mut rows = self
            .person
            .consent
            .iter()
            .filter(|row| row.channel == channel && row.topic == topic);
        if status == ConsentStatus::Unknown {
            rows.all(|row| row.status == ConsentStatus::Unknown)
        } else {
            rows.any(|row| row.status == status)
        }
    }

    fn event(&self, condition: &EventCondition, pointer: String) -> EvalNode {
        if self.person.capped_events.contains(&condition.name) {
            return self.leaf(pointer, Outcome::Unknown, Observed::Missing);
        }
        let Some(schema) = self.catalog.events.get(&condition.name) else {
            return self.leaf(pointer, Outcome::Fail, Observed::Missing);
        };
        // A `since_trait` window needs a readable bound; without one nothing matches, an
        // aggregate included.
        let since = match &condition.window {
            Some(Window::SinceTrait(key)) => {
                let kind = match self.catalog.traits.get(key) {
                    Some(TraitType::Date) => Kind::Date,
                    _ => Kind::Timestamp,
                };
                match stored_time_of(kind, self.person.traits.get(key)) {
                    Some(at) => Some(at),
                    None => {
                        return self.leaf(pointer, Outcome::Fail, Observed::Matching { events: 0 });
                    }
                }
            }
            _ => None,
        };
        let matching: Vec<&SnapshotEvent> = self
            .person
            .events
            .iter()
            .filter(|event| event.name == condition.name)
            .filter(|event| self.in_window(condition.window.as_ref(), since, event))
            .filter(|event| {
                condition.filters.iter().all(|filter| {
                    schema.get(&filter.prop).is_some_and(|kind| {
                        leaf(
                            Kind::from(*kind),
                            event.properties.get(&filter.prop),
                            filter.op,
                            filter.value.as_ref(),
                            self.now,
                        )
                    })
                })
            })
            .collect();
        let events = matching.len();
        let Some(agg) = &condition.agg else {
            return self.leaf(
                pointer,
                Outcome::of(events > 0),
                Observed::Matching { events },
            );
        };
        let (passed, value) = match agg.func {
            AggFn::Count => {
                let count = events as f64;
                (
                    number(Some(count), agg.op, agg.value.as_ref()),
                    Some(Value::from(events)),
                )
            }
            AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::Avg => {
                let integral = agg
                    .prop
                    .as_ref()
                    .is_some_and(|prop| schema.get(prop) == Some(&PropertyType::Integer));
                let values: Vec<f64> = matching
                    .iter()
                    .filter_map(|event| {
                        let prop = agg.prop.as_ref()?;
                        stored_number(event.properties.get(prop), integral)
                    })
                    .collect();
                let folded = fold(agg.func, &values);
                (
                    number(folded, agg.op, agg.value.as_ref()),
                    folded
                        .and_then(|number| serde_json::Number::from_f64(number).map(Value::Number)),
                )
            }
            AggFn::FirstTime | AggFn::LastTime => {
                let times = matching
                    .iter()
                    .map(|event| stored_time::micros(event.occurred_at));
                let at = if agg.func == AggFn::FirstTime {
                    times.min()
                } else {
                    times.max()
                };
                (
                    time(at, Kind::Timestamp, agg.op, agg.value.as_ref(), self.now),
                    at.and_then(rfc3339),
                )
            }
        };
        self.leaf(
            pointer,
            Outcome::of(passed),
            Observed::Aggregate { value, events },
        )
    }

    fn in_window(
        &self,
        window: Option<&Window>,
        since: Option<i64>,
        event: &SnapshotEvent,
    ) -> bool {
        let at = stored_time::micros(event.occurred_at);
        match window {
            None => true,
            Some(Window::Last(text)) => {
                span_of(text).is_some_and(|span| self.now - span < at && at <= self.now)
            }
            Some(Window::Between(start, end)) => {
                let bound = |text: &String| absolute(Kind::Timestamp, &Value::String(text.clone()));
                match (bound(start), bound(end)) {
                    (Some(start), Some(end)) => start <= at && at < end,
                    _ => false,
                }
            }
            Some(Window::SinceTrait(_)) => since.is_some_and(|since| since <= at),
        }
    }
}

/// `sum`, `min`, `max` or `avg` of the readable values; nothing to fold is no value.
fn fold(func: AggFn, values: &[f64]) -> Option<f64> {
    let (first, rest) = values.split_first()?;
    Some(match func {
        AggFn::Sum => rest.iter().fold(*first, |sum, value| sum + value),
        AggFn::Min => rest.iter().fold(*first, |least, value| least.min(*value)),
        AggFn::Max => rest.iter().fold(*first, |most, value| most.max(*value)),
        _ => rest.iter().fold(*first, |sum, value| sum + value) / values.len() as f64,
    })
}

fn rfc3339(micros: i64) -> Option<Value> {
    let nanos = i128::from(micros) * 1_000;
    let at = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
    crate::time_value::format_timestamp(at).map(Value::String)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use hashbrown::HashMap;
    use serde_json::{Map, json};
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::consent::Channel;

    use super::*;
    use crate::snapshot::SnapshotConsent;

    const NOW: OffsetDateTime = datetime!(2025-06-15 12:00:00 UTC);
    const LIST: &str = "018f0000-0000-7000-8000-000000000001";
    const NEWS: &str = "018f0000-0000-7000-8000-000000000004";

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        for (key, kind) in [
            ("name", TraitType::String),
            ("age", TraitType::Number),
            ("signed_up", TraitType::Timestamp),
            ("birthday", TraitType::Date),
        ] {
            catalog.traits.insert(key.to_owned(), kind);
        }
        let mut order = HashMap::new();
        order.insert("total".to_owned(), PropertyType::Number);
        order.insert("items".to_owned(), PropertyType::Integer);
        catalog.events.insert("order".to_owned(), order);
        catalog.lists.insert(Uuid::parse_str(LIST).unwrap());
        let mut topics = HashMap::new();
        topics.insert("news".to_owned(), Uuid::parse_str(NEWS).unwrap());
        catalog.topics.insert(Channel::Email, topics);
        catalog
    }

    fn definition(mut value: Value) -> Definition {
        value["v"] = json!(1);
        Definition::from_value(&value).unwrap()
    }

    fn order(at: OffsetDateTime, properties: Value) -> SnapshotEvent {
        SnapshotEvent {
            name: "order".into(),
            occurred_at: at,
            properties: properties.as_object().cloned().unwrap_or_default(),
        }
    }

    fn run(value: Value, person: &PersonSnapshot) -> Evaluation {
        evaluate(&definition(value), &catalog(), person, NOW)
    }

    fn outcome(value: Value, person: &PersonSnapshot) -> Outcome {
        run(value, person).outcome
    }

    fn traits(value: Value) -> PersonSnapshot {
        PersonSnapshot {
            traits: value.as_object().cloned().unwrap_or_default(),
            ..PersonSnapshot::default()
        }
    }

    #[test]
    fn the_tree_carries_the_pointers_of_the_evaluated_definition() {
        let person = traits(json!({"name": "Ada"}));
        let result = run(
            json!({"all": [
                {"not": {"trait": "name", "op": "eq", "value": "Bob"}},
                {"any": [{"trait": "name", "op": "exists"}]},
            ]}),
            &person,
        );
        assert_eq!(result.outcome, Outcome::Pass);
        let pointers = |node: &EvalNode| -> Vec<String> {
            node.children
                .iter()
                .map(|child| child.pointer.clone())
                .collect()
        };
        assert_eq!(result.tree.pointer, "");
        assert_eq!(pointers(&result.tree), ["/all/0", "/all/1"]);
        assert_eq!(pointers(&result.tree.children[0]), ["/all/0/not"]);
        assert_eq!(pointers(&result.tree.children[1]), ["/all/1/any/0"]);
    }

    #[test]
    fn a_leaf_reports_what_it_saw() {
        let person = traits(json!({"name": 7}));
        let tree = run(json!({"trait": "name", "op": "eq", "value": "x"}), &person).tree;
        assert_eq!(tree.outcome, Outcome::Fail);
        assert_eq!(tree.observed, Some(Observed::Stored(json!(7))));
        let tree = run(json!({"trait": "age", "op": "exists"}), &person).tree;
        assert_eq!(tree.observed, Some(Observed::Missing));
        let tree = run(json!({"list": LIST}), &person).tree;
        assert_eq!(tree.observed, Some(Observed::Present(false)));
    }

    #[test]
    fn not_is_plain_negation_over_two_valued_leaves() {
        let nobody = PersonSnapshot::default();
        assert_eq!(
            outcome(
                json!({"not": {"trait": "name", "op": "eq", "value": "x"}}),
                &nobody
            ),
            Outcome::Pass
        );
        assert_eq!(
            outcome(json!({"trait": "name", "op": "neq", "value": "x"}), &nobody),
            Outcome::Fail
        );
        assert_eq!(outcome(json!({"all": []}), &nobody), Outcome::Pass);
        assert_eq!(outcome(json!({"any": []}), &nobody), Outcome::Fail);
    }

    #[test]
    fn a_zero_event_count_is_zero_and_the_other_aggregates_are_false() {
        let nobody = PersonSnapshot::default();
        let count = json!({"event": "order", "agg": {"fn": "count", "op": "eq", "value": 0}});
        assert_eq!(outcome(count, &nobody), Outcome::Pass);
        for (func, op) in [
            ("sum", "neq"),
            ("avg", "not_in"),
            ("min", "lt"),
            ("max", "gte"),
        ] {
            let value = if op == "not_in" { json!([1]) } else { json!(1) };
            let agg = json!({"fn": func, "prop": "total", "op": op, "value": value});
            assert_eq!(
                outcome(json!({"event": "order", "agg": agg}), &nobody),
                Outcome::Fail,
                "{func} {op}"
            );
        }
        let first = json!({"fn": "first_time", "op": "before", "value": "2030-01-01T00:00:00Z"});
        assert_eq!(
            outcome(json!({"event": "order", "agg": first}), &nobody),
            Outcome::Fail
        );
    }

    #[test]
    fn aggregates_skip_the_events_whose_property_the_sql_cannot_read() {
        let person = PersonSnapshot {
            events: vec![
                order(NOW, json!({"total": 2})),
                order(NOW, json!({"total": "3"})),
                order(NOW, json!({"total": 1e301})),
                order(NOW, json!({})),
                order(NOW, json!({"total": 4.5})),
            ],
            ..PersonSnapshot::default()
        };
        let agg = |func: &str, op: &str, value: Value| json!({"event": "order", "agg": {"fn": func, "prop": "total", "op": op, "value": value}});
        assert_eq!(
            outcome(agg("sum", "eq", json!(6.5)), &person),
            Outcome::Pass
        );
        assert_eq!(
            outcome(agg("avg", "eq", json!(3.25)), &person),
            Outcome::Pass
        );
        assert_eq!(outcome(agg("min", "eq", json!(2)), &person), Outcome::Pass);
        assert_eq!(
            outcome(agg("max", "eq", json!(4.5)), &person),
            Outcome::Pass
        );
        let count = json!({"event": "order", "agg": {"fn": "count", "op": "eq", "value": 5}});
        assert_eq!(outcome(count, &person), Outcome::Pass);
    }

    #[test]
    fn an_integer_property_reads_only_whole_numbers() {
        let person = PersonSnapshot {
            events: vec![
                order(NOW, json!({"items": 2.5})),
                order(NOW, json!({"items": 3.0})),
            ],
            ..PersonSnapshot::default()
        };
        let sum = json!({"event": "order", "agg": {"fn": "sum", "prop": "items", "op": "eq", "value": 3}});
        assert_eq!(outcome(sum, &person), Outcome::Pass);
        let filter =
            json!({"event": "order", "where": [{"prop": "items", "op": "eq", "value": 2}]});
        assert_eq!(outcome(filter, &person), Outcome::Fail);
    }

    #[test]
    fn windows_include_their_end_and_since_the_start_as_the_sql_does() {
        let span = time::Duration::days(1);
        let at = |event: OffsetDateTime, window: Value| {
            let person = PersonSnapshot {
                events: vec![order(event, json!({}))],
                ..PersonSnapshot::default()
            };
            outcome(json!({"event": "order", "window": window}), &person)
        };
        let last = json!({"last": "P1D"});
        assert_eq!(at(NOW - span, last.clone()), Outcome::Fail);
        assert_eq!(
            at(NOW - span + time::Duration::SECOND, last.clone()),
            Outcome::Pass
        );
        assert_eq!(at(NOW, last.clone()), Outcome::Pass);
        assert_eq!(at(NOW + time::Duration::SECOND, last), Outcome::Fail);
        let between = json!({"between": ["2025-06-01T00:00:00Z", "2025-06-02T00:00:00Z"]});
        assert_eq!(
            at(datetime!(2025-06-01 0:00 UTC), between.clone()),
            Outcome::Pass
        );
        assert_eq!(at(datetime!(2025-06-02 0:00 UTC), between), Outcome::Fail);
    }

    #[test]
    fn a_missing_or_invalid_since_trait_fails_the_whole_condition_even_an_aggregate() {
        let events = vec![order(NOW, json!({}))];
        let zero = json!({"event": "order", "window": {"since_trait": "signed_up"},
            "agg": {"fn": "count", "op": "eq", "value": 0}});
        let plain = json!({"event": "order", "window": {"since_trait": "signed_up"}});
        for stored in [
            traits(json!({})),
            traits(json!({"signed_up": null})),
            traits(json!({"signed_up": "infinity"})),
            traits(json!({"signed_up": 5})),
            traits(json!({"signed_up": "2025-06-14"})),
        ] {
            let person = PersonSnapshot {
                events: events.clone(),
                ..stored
            };
            assert_eq!(outcome(zero.clone(), &person), Outcome::Fail);
            assert_eq!(outcome(plain.clone(), &person), Outcome::Fail);
        }
        let person = PersonSnapshot {
            events: events.clone(),
            ..traits(json!({"signed_up": "2025-06-15T12:00:00Z"}))
        };
        assert_eq!(outcome(plain.clone(), &person), Outcome::Pass);
        let person = PersonSnapshot {
            events,
            ..traits(json!({"signed_up": "2025-06-15T12:00:01Z"}))
        };
        assert_eq!(outcome(plain, &person), Outcome::Fail);
    }

    #[test]
    fn a_date_trait_bounds_a_window_at_utc_midnight() {
        let person = PersonSnapshot {
            events: vec![order(datetime!(2025-06-10 0:00 UTC), json!({}))],
            ..traits(json!({"birthday": "2025-06-10"}))
        };
        let since = json!({"event": "order", "window": {"since_trait": "birthday"}});
        assert_eq!(outcome(since, &person), Outcome::Pass);
    }

    #[test]
    fn consent_unknown_means_no_row_or_an_unknown_row() {
        let topic = Uuid::parse_str(NEWS).unwrap();
        let row = |status, topic| SnapshotConsent {
            channel: Channel::Email,
            topic,
            status,
        };
        let unknown = json!({"consent": {"channel": "email", "status": "unknown"}});
        let on_topic =
            json!({"consent": {"channel": "email", "topic": "news", "status": "subscribed"}});
        let none = PersonSnapshot::default();
        assert_eq!(outcome(unknown.clone(), &none), Outcome::Pass);
        let channel = PersonSnapshot {
            consent: vec![row(ConsentStatus::Subscribed, None)],
            ..PersonSnapshot::default()
        };
        assert_eq!(outcome(unknown.clone(), &channel), Outcome::Fail);
        assert_eq!(outcome(on_topic.clone(), &channel), Outcome::Fail);
        let topical = PersonSnapshot {
            consent: vec![row(ConsentStatus::Subscribed, Some(topic))],
            ..PersonSnapshot::default()
        };
        assert_eq!(outcome(unknown, &topical), Outcome::Pass);
        assert_eq!(outcome(on_topic, &topical), Outcome::Pass);
    }

    #[test]
    fn references_read_the_snapshot_sets() {
        let list = Uuid::parse_str(LIST).unwrap();
        let person = PersonSnapshot {
            lists: BTreeSet::from([list]),
            identifier_kinds: BTreeSet::from(["email".to_owned()]),
            ..PersonSnapshot::default()
        };
        assert_eq!(outcome(json!({"list": LIST}), &person), Outcome::Pass);
        assert_eq!(
            outcome(json!({"identifier": "email"}), &person),
            Outcome::Pass
        );
        assert_eq!(
            outcome(json!({"identifier": "phone"}), &person),
            Outcome::Fail
        );
    }

    #[test]
    fn a_capped_event_is_unknown_and_three_valued_logic_lets_a_known_child_decide() {
        let person = PersonSnapshot {
            capped_events: BTreeSet::from(["order".to_owned()]),
            ..PersonSnapshot::default()
        };
        let capped = json!({"event": "order"});
        let known_false = json!({"trait": "name", "op": "exists"});
        let known_true = json!({"not": {"trait": "name", "op": "exists"}});
        assert_eq!(outcome(capped.clone(), &person), Outcome::Unknown);
        assert_eq!(
            outcome(json!({"not": capped.clone()}), &person),
            Outcome::Unknown
        );
        assert_eq!(
            outcome(
                json!({"all": [capped.clone(), known_false.clone()]}),
                &person
            ),
            Outcome::Fail
        );
        assert_eq!(
            outcome(
                json!({"all": [capped.clone(), known_true.clone()]}),
                &person
            ),
            Outcome::Unknown
        );
        assert_eq!(
            outcome(json!({"any": [capped.clone(), known_true]}), &person),
            Outcome::Pass
        );
        assert_eq!(
            outcome(json!({"any": [capped, known_false]}), &person),
            Outcome::Unknown
        );
    }

    #[test]
    fn a_cap_on_another_event_decides_nothing() {
        let mut person = PersonSnapshot::default();
        person.capped_events.insert("other".to_owned());
        person.events.push(order(NOW, Map::new().into()));
        assert_eq!(outcome(json!({"event": "order"}), &person), Outcome::Pass);
    }
}
