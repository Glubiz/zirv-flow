//! What a caller without access to personal values may put in a segment definition.
//!
//! A count or a member list answers "does this person match?", so a condition that orders or
//! prefixes a personal value lets a caller read the value one probe at a time. People search and
//! the explorer refuse the same probes; this is the same rule for segments, applied to a
//! validated definition before it is previewed or saved. Only `eq`, `exists` and `not_exists`
//! may name a personal trait or event property. A trait or property the workspace has not
//! declared not-personal counts as personal.

use zirv_flow_core::segment::{AggFn, Node, Op, Window};
use zirv_flow_store::ClearFields;

use crate::problem::ApiError;

fn is_presence_or_equality(op: Op) -> bool {
    matches!(op, Op::Eq | Op::Exists | Op::NotExists)
}

/// Refuses a condition on a personal trait or event property that is not `eq`, `exists` or
/// `not_exists`: 403 `pii_trait_not_searchable` for a trait, 403 `pii_property_not_explorable`
/// for an event property. A caller who may see personal values (`reveal`) is never refused, and
/// nothing is read for them.
pub(crate) async fn refuse_personal_probes(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    node: &Node,
    reveal: bool,
) -> Result<(), ApiError> {
    refuse_personal_probe_roots(connection, workspace, std::slice::from_ref(node), reveal).await
}

/// [`refuse_personal_probes`] for several conditions, reading what is personal once: a flow holds
/// many.
pub(crate) async fn refuse_personal_probe_roots(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    roots: &[Node],
    reveal: bool,
) -> Result<(), ApiError> {
    if reveal || roots.is_empty() {
        return Ok(());
    }
    let clear = zirv_flow_store::explain_clear_fields(connection, workspace)
        .await
        .map_err(super::preview::read_error)?;
    roots.iter().try_for_each(|root| check(root, &clear))
}

/// A trait key a webhook or sync node sends out.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TraitSend {
    pub(crate) node: String,
    pub(crate) key: String,
}

/// A published webhook template a webhook node sends.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemplateSend {
    pub(crate) node: String,
    pub(crate) template: uuid::Uuid,
}

/// What a flow check found that leaves the system, for the guard to judge.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Sends {
    pub(crate) traits: Vec<TraitSend>,
    pub(crate) templates: Vec<TemplateSend>,
}

/// Refuses a flow that would send personal values out for a caller who may not see them: a
/// webhook or sync node whose `trait_keys` names a personal trait, or a webhook node whose
/// published template reads a personal field. 403 `pii_not_sendable`. A caller who may see
/// personal values (`reveal`) is never refused, and nothing is read for them.
pub(crate) async fn refuse_personal_sends(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    sends: &Sends,
    reveal: bool,
) -> Result<(), ApiError> {
    if reveal || (sends.traits.is_empty() && sends.templates.is_empty()) {
        return Ok(());
    }
    let clear = zirv_flow_store::explain_clear_fields(connection, workspace)
        .await
        .map_err(super::preview::read_error)?;
    check_trait_sends(&sends.traits, &clear)?;
    crate::templates::refuse_personal_templates(connection, workspace, &sends.templates, &clear)
        .await
}

fn check_trait_sends(sends: &[TraitSend], clear: &ClearFields) -> Result<(), ApiError> {
    match sends.iter().find(|send| clear.is_trait_pii(&send.key)) {
        Some(send) => Err(ApiError::pii_not_sendable(Some(&send.node), &send.key)),
        None => Ok(()),
    }
}

fn check(node: &Node, clear: &ClearFields) -> Result<(), ApiError> {
    match node {
        Node::All(children) | Node::Any(children) => {
            children.iter().try_for_each(|child| check(child, clear))
        }
        Node::Not(child) => check(child, clear),
        Node::Trait(condition) => {
            if clear.is_trait_pii(&condition.key) && !is_presence_or_equality(condition.op) {
                return Err(ApiError::pii_trait_not_searchable());
            }
            Ok(())
        }
        Node::Event(condition) => {
            for filter in &condition.filters {
                if clear.is_property_pii(&condition.name, &filter.prop)
                    && !is_presence_or_equality(filter.op)
                {
                    return Err(ApiError::pii_property_not_explorable());
                }
            }
            // `count`, `first_time` and `last_time` read no property value.
            if let Some(agg) = &condition.agg
                && matches!(agg.func, AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::Avg)
                && agg
                    .prop
                    .as_deref()
                    .is_none_or(|prop| clear.is_property_pii(&condition.name, prop))
            {
                return Err(ApiError::pii_property_not_explorable());
            }
            // A window that starts at a personal date trait orders that value.
            if let Some(Window::SinceTrait(key)) = &condition.window
                && clear.is_trait_pii(key)
            {
                return Err(ApiError::pii_trait_not_searchable());
            }
            Ok(())
        }
        Node::List(_)
        | Node::Segment(_)
        | Node::Consent(_)
        | Node::Identifier(_)
        | Node::Messages(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use zirv_flow_segments::{Catalog, parse_and_validate};

    use super::*;

    fn catalog() -> Catalog {
        use hashbrown::HashMap;
        use zirv_flow_core::events::PropertyType;
        use zirv_flow_core::people::TraitType;
        Catalog {
            traits: HashMap::from([
                ("ssn".to_owned(), TraitType::String),
                ("plan".to_owned(), TraitType::String),
                ("born".to_owned(), TraitType::Timestamp),
            ]),
            events: HashMap::from([(
                "order".to_owned(),
                HashMap::from([
                    ("total".to_owned(), PropertyType::Number),
                    ("sku".to_owned(), PropertyType::String),
                ]),
            )]),
            ..Catalog::default()
        }
    }

    fn clear() -> ClearFields {
        ClearFields::new(["plan"], [("order", "sku")])
    }

    fn verdict(definition: Value, reveal: bool) -> Result<(), ApiError> {
        let parsed = parse_and_validate(&definition, &catalog()).expect("valid");
        if reveal {
            Ok(())
        } else {
            check(&parsed.root, &clear())
        }
    }

    #[test]
    fn only_eq_exists_and_not_exists_name_a_personal_value() {
        for op in ["eq", "exists", "not_exists"] {
            let mut definition = json!({"v": 1, "trait": "ssn", "op": op});
            if op == "eq" {
                definition["value"] = json!("x");
            }
            verdict(definition, false).unwrap();
        }
        let prefix = json!({"v": 1, "trait": "ssn", "op": "starts_with", "value": "1"});
        let error = verdict(prefix.clone(), false).unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.code(), Some("pii_trait_not_searchable"));
        verdict(prefix, true).unwrap();
    }

    #[test]
    fn a_probe_is_found_under_all_any_and_not() {
        let nested = json!({"v": 1, "all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"not": {"any": [{"trait": "ssn", "op": "starts_with", "value": "1"}]}}
        ]});
        assert!(verdict(nested, false).is_err());
    }

    #[test]
    fn a_clear_trait_and_property_are_unrestricted() {
        verdict(
            json!({"v": 1, "trait": "plan", "op": "starts_with", "value": "p"}),
            false,
        )
        .unwrap();
        verdict(
            json!({"v": 1, "event": "order",
                   "where": [{"prop": "sku", "op": "starts_with", "value": "a"}]}),
            false,
        )
        .unwrap();
    }

    #[test]
    fn a_personal_property_filter_and_value_aggregate_are_refused_but_count_is_not() {
        let range = json!({"v": 1, "event": "order",
                           "where": [{"prop": "total", "op": "gt", "value": 1}]});
        assert_eq!(
            verdict(range, false).unwrap_err().code(),
            Some("pii_property_not_explorable")
        );
        let sum = json!({"v": 1, "event": "order",
                         "agg": {"fn": "sum", "prop": "total", "op": "gt", "value": 1}});
        assert!(verdict(sum, false).is_err());
        let count = json!({"v": 1, "event": "order",
                           "agg": {"fn": "count", "op": "gte", "value": 1}});
        verdict(count, false).unwrap();
    }

    #[test]
    fn a_personal_trait_key_is_not_sendable_and_a_clear_or_unknown_one_is_judged_by_the_registry() {
        let send = |node: &str, key: &str| TraitSend {
            node: node.to_owned(),
            key: key.to_owned(),
        };

        check_trait_sends(&[send("a", "plan")], &clear()).unwrap();
        let error =
            check_trait_sends(&[send("a", "plan"), send("b", "ssn")], &clear()).unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.code(), Some("pii_not_sendable"));
        assert!(error.detail().contains("`b`"), "{}", error.detail());
        // A key the registry does not know counts as personal.
        assert!(check_trait_sends(&[send("c", "nope")], &clear()).is_err());
    }

    #[test]
    fn a_window_from_a_personal_date_trait_is_refused() {
        let window = json!({"v": 1, "event": "order", "window": {"since_trait": "born"}});
        assert_eq!(
            verdict(window, false).unwrap_err().code(),
            Some("pii_trait_not_searchable")
        );
    }
}
