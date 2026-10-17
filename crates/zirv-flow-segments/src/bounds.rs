//! The events a definition reads, and how far back (architecture section 8.3): what the
//! explain view must load of one person so the evaluator sees every event a condition can use.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use zirv_flow_core::segment::{Definition, Node, Window};

use crate::leaf::span_of;
use crate::operand;

const MICROS_TO_NANOS: i128 = 1_000;

/// Each event name the definition reads, with the earliest `occurred_at` any condition on it can
/// use: `last P` reads from `now - P`, `between` from its start. `None` is no lower bound, for a
/// `since_trait` window, no window, or a window that cannot be read; it wins over any bound, so
/// the bound is the widest one.
pub fn event_lower_bounds(
    definition: &Definition,
    now: OffsetDateTime,
) -> BTreeMap<String, Option<OffsetDateTime>> {
    let mut bounds = BTreeMap::new();
    collect(&definition.root, now, &mut bounds);
    bounds
}

fn collect(
    node: &Node,
    now: OffsetDateTime,
    bounds: &mut BTreeMap<String, Option<OffsetDateTime>>,
) {
    match node {
        Node::All(children) | Node::Any(children) => {
            for child in children {
                collect(child, now, bounds);
            }
        }
        Node::Not(child) => collect(child, now, bounds),
        Node::Event(condition) => {
            let bound = lower_bound(condition.window.as_ref(), now);
            bounds
                .entry(condition.name.clone())
                .and_modify(|held| *held = widest(*held, bound))
                .or_insert(bound);
        }
        Node::Trait(_)
        | Node::List(_)
        | Node::Segment(_)
        | Node::Consent(_)
        | Node::Identifier(_) => {}
    }
}

fn widest(a: Option<OffsetDateTime>, b: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    Some(a?.min(b?))
}

fn lower_bound(window: Option<&Window>, now: OffsetDateTime) -> Option<OffsetDateTime> {
    match window? {
        Window::Last(text) => {
            let micros = crate::stored_time::micros(now).checked_sub(span_of(text)?)?;
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * MICROS_TO_NANOS).ok()
        }
        Window::Between(start, _) => operand::timestamp(start),
        Window::SinceTrait(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use time::macros::datetime;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2025-06-15 12:00:00 UTC);

    fn bounds(root: Value) -> BTreeMap<String, Option<OffsetDateTime>> {
        let mut value = root;
        value["v"] = json!(1);
        event_lower_bounds(&Definition::from_value(&value).unwrap(), NOW)
    }

    fn event(name: &str, window: Value) -> Value {
        match window {
            Value::Null => json!({"event": name}),
            window => json!({"event": name, "window": window}),
        }
    }

    #[test]
    fn a_last_window_reads_from_now_minus_its_span() {
        let held = bounds(event("order", json!({"last": "P7D"})));

        assert_eq!(held["order"], Some(datetime!(2025-06-08 12:00:00 UTC)));
    }

    #[test]
    fn a_between_window_reads_from_its_start() {
        let held = bounds(event(
            "order",
            json!({"between": ["2025-01-01T00:00:00Z", "2025-02-01T00:00:00Z"]}),
        ));

        assert_eq!(held["order"], Some(datetime!(2025-01-01 00:00:00 UTC)));
    }

    #[test]
    fn no_window_and_a_since_trait_window_have_no_bound() {
        let held = bounds(json!({"all": [
            event("order", Value::Null),
            event("visit", json!({"since_trait": "signed_up"})),
        ]}));

        assert_eq!(held["order"], None);
        assert_eq!(held["visit"], None);
    }

    #[test]
    fn the_widest_bound_of_one_name_wins_and_no_bound_beats_any() {
        let held = bounds(json!({"any": [
            event("order", json!({"last": "P1D"})),
            {"not": event("order", json!({"last": "P30D"}))},
            event("visit", json!({"last": "P1D"})),
            event("visit", Value::Null),
        ]}));

        assert_eq!(held["order"], Some(datetime!(2025-05-16 12:00:00 UTC)));
        assert_eq!(held["visit"], None);
        assert_eq!(held.len(), 2);
    }

    #[test]
    fn a_definition_without_events_reads_none() {
        assert!(bounds(json!({"list": "018f0000-0000-7000-8000-000000000001"})).is_empty());
    }
}
