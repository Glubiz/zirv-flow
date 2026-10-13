//! Rewrites a valid definition into one canonical form (D6) without changing what it selects.
//!
//! `not` is pushed inward through `all` and `any` only. A leaf is never rewritten under `not`:
//! two-valued logic makes `not (country eq DK)` select people with no country, which
//! `country neq DK` does not.

use serde_json::Value;
use zirv_flow_core::segment::{Aggregate, Definition, EventCondition, Node, Op, Window};

use crate::time_value::{format_duration, format_timestamp, parse_duration, parse_timestamp};

/// The canonical form of a definition. Normalizing the result again changes nothing.
pub fn normalize(definition: Definition) -> Definition {
    Definition {
        v: definition.v,
        root: node(definition.root, false),
    }
}

fn node(current: Node, negated: bool) -> Node {
    match current {
        Node::All(children) => group(!negated, children, negated),
        Node::Any(children) => group(negated, children, negated),
        Node::Not(inner) => node(*inner, !negated),
        leaf => {
            let leaf = canonical_leaf(leaf);
            if negated {
                Node::Not(Box::new(leaf))
            } else {
                leaf
            }
        }
    }
}

/// Builds the `all` (or `any`) of the children, each normalized under the pending negation.
/// An empty `all` is true and an empty `any` is false: the first is the identity of `all` and
/// absorbs an `any`, and the other way round.
fn group(all: bool, children: Vec<Node>, negated: bool) -> Node {
    let mut flat = Vec::with_capacity(children.len());
    for child in children {
        match (all, node(child, negated)) {
            (true, Node::Any(inner)) if inner.is_empty() => return Node::Any(Vec::new()),
            (false, Node::All(inner)) if inner.is_empty() => return Node::All(Vec::new()),
            (true, Node::All(inner)) | (false, Node::Any(inner)) => flat.extend(inner),
            (_, other) => flat.push(other),
        }
    }
    sort_unique(&mut flat);
    match flat.len() {
        0 if all => Node::All(flat),
        0 => Node::Any(flat),
        1 => flat.remove(0),
        _ if all => Node::All(flat),
        _ => Node::Any(flat),
    }
}

fn sort_unique<T>(items: &mut Vec<T>)
where
    T: PartialEq + Keyed,
{
    items.sort_by_cached_key(Keyed::key);
    items.dedup();
}

trait Keyed {
    fn key(&self) -> String;
}

impl Keyed for Node {
    fn key(&self) -> String {
        self.to_value().to_string()
    }
}

impl Keyed for zirv_flow_core::segment::PropertyFilter {
    fn key(&self) -> String {
        format!(
            "{}\u{0}{}\u{0}{:?}",
            self.prop,
            self.op.as_str(),
            self.value
        )
    }
}

fn canonical_leaf(leaf: Node) -> Node {
    match leaf {
        Node::Trait(mut condition) => {
            canonical_value(condition.op, &mut condition.value);
            Node::Trait(condition)
        }
        Node::Event(condition) => Node::Event(canonical_event(condition)),
        other => other,
    }
}

fn canonical_event(mut condition: EventCondition) -> EventCondition {
    for filter in &mut condition.filters {
        canonical_value(filter.op, &mut filter.value);
    }
    sort_unique(&mut condition.filters);
    if let Some(Aggregate { op, value, .. }) = &mut condition.agg {
        canonical_value(*op, value);
    }
    condition.window = condition.window.map(|window| match window {
        Window::Last(text) => Window::Last(canonical_duration(text)),
        Window::Between(start, end) => {
            Window::Between(canonical_timestamp(start), canonical_timestamp(end))
        }
        since => since,
    });
    condition
}

fn canonical_value(op: Op, value: &mut Option<Value>) {
    match (op, value) {
        (Op::In | Op::NotIn | Op::Contains | Op::ContainsAll, Some(Value::Array(items))) => {
            items.sort_by_cached_key(Value::to_string);
            items.dedup();
        }
        (Op::Before | Op::After, Some(Value::String(text))) => {
            *text = canonical_timestamp(std::mem::take(text));
        }
        (Op::Between, Some(Value::Array(items))) => {
            for item in items {
                if let Value::String(text) = item {
                    *text = canonical_timestamp(std::mem::take(text));
                }
            }
        }
        (Op::WithinLast | Op::MoreThanAgo | Op::WithinNext, Some(Value::String(text))) => {
            *text = canonical_duration(std::mem::take(text));
        }
        _ => {}
    }
}

/// Dates and anything that is not RFC 3339 are left as written.
fn canonical_timestamp(text: String) -> String {
    parse_timestamp(&text)
        .and_then(format_timestamp)
        .unwrap_or(text)
}

fn canonical_duration(text: String) -> String {
    parse_duration(&text).map_or(text, format_duration)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn norm(mut value: Value) -> Value {
        value["v"] = json!(1);
        let definition = Definition::from_value(&value).expect("the fixture should parse");
        let normalized = normalize(definition);
        let twice = normalize(normalized.clone());
        assert_eq!(normalized, twice, "normalizing twice must change nothing");
        normalized.to_value()
    }

    fn a() -> Value {
        json!({"trait": "a", "op": "exists"})
    }
    fn b() -> Value {
        json!({"trait": "b", "op": "exists"})
    }
    fn c() -> Value {
        json!({"trait": "c", "op": "exists"})
    }

    fn with_v(mut value: Value) -> Value {
        value["v"] = json!(1);
        value
    }

    #[test]
    fn nested_groups_of_the_same_kind_are_flattened() {
        assert_eq!(
            norm(json!({"all": [a(), {"all": [b(), {"all": [c()]}]}]})),
            with_v(json!({"all": [a(), b(), c()]}))
        );
        assert_eq!(
            norm(json!({"any": [c(), {"any": [b(), a()]}]})),
            with_v(json!({"any": [a(), b(), c()]}))
        );
    }

    #[test]
    fn a_group_with_one_child_becomes_the_child() {
        assert_eq!(norm(json!({"all": [a()]})), with_v(a()));
        assert_eq!(norm(json!({"any": [{"all": [b()]}]})), with_v(b()));
    }

    #[test]
    fn not_is_pushed_through_all_and_any() {
        assert_eq!(
            norm(json!({"not": {"all": [a(), b()]}})),
            with_v(json!({"any": [{"not": a()}, {"not": b()}]}))
        );
        assert_eq!(
            norm(json!({"not": {"any": [a(), {"all": [b(), c()]}]}})),
            with_v(json!({"all": [
                {"any": [{"not": b()}, {"not": c()}]},
                {"not": a()},
            ]}))
        );
    }

    #[test]
    fn double_negation_is_removed() {
        assert_eq!(norm(json!({"not": {"not": a()}})), with_v(a()));
        assert_eq!(
            norm(json!({"not": {"not": {"not": a()}}})),
            with_v(json!({"not": a()}))
        );
        assert_eq!(
            norm(json!({"not": {"all": [{"not": a()}, {"not": b()}]}})),
            with_v(json!({"any": [a(), b()]}))
        );
    }

    #[test]
    fn a_leaf_under_not_keeps_its_operator() {
        let eq = json!({"trait": "country", "op": "eq", "value": "DK"});
        assert_eq!(
            norm(json!({"not": eq.clone()})),
            with_v(json!({"not": eq.clone()}))
        );
        assert_eq!(
            norm(json!({"not": {"all": [eq.clone(), a()]}})),
            with_v(json!({"any": [{"not": eq}, {"not": a()}]}))
        );
        let before = json!({"trait": "t", "op": "before", "value": "2024-01-01T00:00:00Z"});
        assert_eq!(
            norm(json!({"not": before.clone()})),
            with_v(json!({"not": before}))
        );
        let window = json!({"event": "e", "window": {"last": "P30D"}});
        assert_eq!(
            norm(json!({"not": window.clone()})),
            with_v(json!({"not": window}))
        );
    }

    #[test]
    fn constants_fold_through_their_parents() {
        assert_eq!(norm(json!({"all": []})), with_v(json!({"all": []})));
        assert_eq!(norm(json!({"any": []})), with_v(json!({"any": []})));
        assert_eq!(norm(json!({"all": [a(), {"all": []}]})), with_v(a()));
        assert_eq!(norm(json!({"any": [a(), {"any": []}]})), with_v(a()));
        assert_eq!(
            norm(json!({"all": [a(), {"any": []}]})),
            with_v(json!({"any": []}))
        );
        assert_eq!(
            norm(json!({"any": [a(), {"all": []}]})),
            with_v(json!({"all": []}))
        );
        assert_eq!(
            norm(json!({"not": {"all": []}})),
            with_v(json!({"any": []}))
        );
        assert_eq!(
            norm(json!({"not": {"any": []}})),
            with_v(json!({"all": []}))
        );
        assert_eq!(
            norm(json!({"all": [a(), {"not": {"any": []}}, {"any": [b(), {"any": []}]}]})),
            with_v(json!({"all": [a(), b()]}))
        );
    }

    #[test]
    fn children_are_ordered_and_exact_duplicates_removed() {
        assert_eq!(
            norm(json!({"all": [c(), a(), b(), a()]})),
            with_v(json!({"all": [a(), b(), c()]}))
        );
        assert_eq!(
            norm(json!({"any": [b(), a(), {"any": [a()]}]})),
            with_v(json!({"any": [a(), b()]}))
        );
        assert_eq!(norm(json!({"all": [a(), a()]})), with_v(a()));
    }

    #[test]
    fn the_order_does_not_depend_on_the_input_order() {
        let one = norm(json!({"all": [a(), {"not": b()}, c(), {"any": [a(), b()]}]}));
        let other = norm(json!({"all": [{"any": [b(), a()]}, c(), {"not": b()}, a()]}));
        assert_eq!(one, other);
    }

    #[test]
    fn values_are_canonicalized() {
        assert_eq!(
            norm(json!({"trait": "x", "op": "in", "value": ["b", "a", "b"]})),
            with_v(json!({"trait": "x", "op": "in", "value": ["a", "b"]}))
        );
        assert_eq!(
            norm(json!({"trait": "x", "op": "contains_all", "value": ["z", "y", "z"]})),
            with_v(json!({"trait": "x", "op": "contains_all", "value": ["y", "z"]}))
        );
        assert_eq!(
            norm(json!({"trait": "t", "op": "after", "value": "2024-03-01T02:00:00+02:00"})),
            with_v(json!({"trait": "t", "op": "after", "value": "2024-03-01T00:00:00Z"}))
        );
        assert_eq!(
            norm(json!({"trait": "t", "op": "between",
                "value": ["2024-03-01T02:00:00+02:00", "2024-04-01T00:00:00Z"]})),
            with_v(json!({"trait": "t", "op": "between",
                "value": ["2024-03-01T00:00:00Z", "2024-04-01T00:00:00Z"]}))
        );
        assert_eq!(
            norm(json!({"trait": "d", "op": "before", "value": "2000-01-01"})),
            with_v(json!({"trait": "d", "op": "before", "value": "2000-01-01"}))
        );
        assert_eq!(
            norm(json!({"trait": "t", "op": "within_last", "value": "P1DT12H"})),
            with_v(json!({"trait": "t", "op": "within_last", "value": "PT36H"}))
        );
        assert_eq!(
            norm(json!({"trait": "t", "op": "more_than_ago", "value": "PT48H"})),
            with_v(json!({"trait": "t", "op": "more_than_ago", "value": "P2D"}))
        );
    }

    #[test]
    fn event_parts_are_canonicalized() {
        let before = json!({"event": "order",
            "where": [
                {"prop": "z", "op": "in", "value": [2, 1]},
                {"prop": "a", "op": "exists"},
                {"prop": "a", "op": "exists"},
            ],
            "agg": {"fn": "last_time", "op": "after", "value": "2024-03-01T02:00:00+02:00"},
            "window": {"between": ["2024-03-01T02:00:00+02:00", "2024-04-01T00:00:00Z"]}});
        let after = json!({"event": "order",
            "where": [
                {"prop": "a", "op": "exists"},
                {"prop": "z", "op": "in", "value": [1, 2]},
            ],
            "agg": {"fn": "last_time", "op": "after", "value": "2024-03-01T00:00:00Z"},
            "window": {"between": ["2024-03-01T00:00:00Z", "2024-04-01T00:00:00Z"]}});
        assert_eq!(norm(before), with_v(after));
        assert_eq!(
            norm(json!({"event": "e", "window": {"last": "PT24H"}})),
            with_v(json!({"event": "e", "window": {"last": "P1D"}}))
        );
    }

    #[test]
    fn references_and_consent_are_left_alone() {
        let id = "018f0000-0000-7000-8000-000000000001";
        let nodes = [
            json!({"list": id}),
            json!({"segment": id}),
            json!({"identifier": "email"}),
            json!({"consent": {"channel": "email", "topic": "news", "status": "subscribed"}}),
        ];
        for node in nodes {
            assert_eq!(norm(node.clone()), with_v(node));
        }
    }

    #[test]
    fn a_definition_at_the_limits_normalizes() {
        let mut deep = a();
        for _ in 0..4 {
            deep = json!({"not": {"all": [deep, b()]}});
        }
        let value = norm(deep);
        assert!(value.is_object());
    }
}
