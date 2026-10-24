//! What a consent policy rule reads beyond the person: the message being sent and the messages
//! already sent (architecture 10.1). A segment has neither, so the leaves that need them are
//! valid in a policy only. The caller loads the facts; nothing here touches a database.

use time::OffsetDateTime;
use zirv_flow_core::consent::policy::PolicyRule;
use zirv_flow_core::consent::{Channel, Dimensions, RecordedConsent};
use zirv_flow_core::segment::Definition;

use crate::catalog::Catalog;
use crate::compile::compile_policy;
use crate::query::Query;
use crate::storage::TraitStorage;

/// The send a policy is evaluated for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendContext {
    pub channel: Channel,
    /// The message's dimension values, the topic not among them.
    pub dimensions: Dimensions,
    /// What the person's records say about this message.
    pub consent: RecordedConsent,
}

/// One message that was sent to the person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotMessage {
    pub channel: Channel,
    pub dimensions: Dimensions,
    /// When the message ended as `sent`.
    pub sent_at: OffsetDateTime,
}

/// The facts the evaluator reads for a policy rule, beside the person's snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyFacts {
    pub send: SendContext,
    /// The sent messages of the person and of every person merged into them (a merge keeps a
    /// message on the id it was written for), test sends excluded.
    pub messages: Vec<SnapshotMessage>,
}

/// How a rule is checked for a person.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleKind {
    /// The built-in rule: it matches when the recorded consent does not allow the send.
    RecordedConsent,
    /// A compiled condition: it matches when the batch query selects the person.
    Query(Query),
    /// A condition that no longer compiles, for example because something it reads was archived
    /// since it was published. The caller treats it as a match, so the send is stopped rather
    /// than let through.
    Unevaluable,
}

/// A rule of a policy, compiled for one send.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledRule {
    pub id: String,
    pub name: String,
    pub kind: RuleKind,
}

/// Compiles the rules of a policy version, in their order, for the send `send` at `now`. A
/// rule's query is the batch form of a compiled segment: bind the person as the last parameter.
pub fn compile_rules(
    rules: &[PolicyRule],
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
    send: &SendContext,
) -> Vec<CompiledRule> {
    rules
        .iter()
        .map(|rule| {
            let kind = match &rule.condition {
                None => RuleKind::RecordedConsent,
                Some(condition) => Definition::from_value(condition)
                    .ok()
                    .and_then(|definition| {
                        compile_policy(&definition, catalog, storage, now, send).ok()
                    })
                    .map_or(RuleKind::Unevaluable, |compiled| {
                        RuleKind::Query(compiled.batch_sql())
                    }),
            };
            CompiledRule {
                id: rule.id.clone(),
                name: rule.name.clone(),
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    fn send(consent: RecordedConsent) -> SendContext {
        SendContext {
            channel: Channel::Email,
            dimensions: Dimensions::new(),
            consent,
        }
    }

    fn rule(id: &str, condition: serde_json::Value) -> PolicyRule {
        PolicyRule {
            id: id.to_owned(),
            name: format!("Rule {id}"),
            condition: Some(condition),
        }
    }

    fn compiled(rules: &[PolicyRule], consent: RecordedConsent) -> Vec<CompiledRule> {
        compile_rules(
            rules,
            &Catalog::default(),
            &TraitStorage::default(),
            datetime!(2026-01-01 12:00 UTC),
            &send(consent),
        )
    }

    #[test]
    fn the_rules_are_compiled_in_their_order_with_the_built_in_one_where_it_was_put() {
        let rules = [
            rule("a", json!({"v": 1, "consent": {"status": "none"}})),
            PolicyRule::recorded_consent(),
            rule("b", json!({"v": 1, "consent": {"status": "denied"}})),
        ];

        let ids: Vec<String> = compiled(&rules, RecordedConsent::None)
            .into_iter()
            .map(|rule| rule.id)
            .collect();

        assert_eq!(ids, ["a", "recorded_consent", "b"]);
    }

    #[test]
    fn a_rule_that_compiles_is_a_batch_query_and_one_that_does_not_is_unevaluable() {
        let rules = [
            rule("good", json!({"v": 1, "consent": {"status": "none"}})),
            rule("archived", json!({"v": 1, "trait": "gone", "op": "exists"})),
            rule("junk", json!({"nope": true})),
            rule(
                "segment_only",
                json!({"v": 1, "consent": {"channel": "email", "status": "granted"}}),
            ),
        ];

        let kinds: Vec<RuleKind> = compiled(&rules, RecordedConsent::None)
            .into_iter()
            .map(|rule| rule.kind)
            .collect();

        assert!(matches!(&kinds[0], RuleKind::Query(query) if query.sql.contains("p.id = ANY(")));
        assert_eq!(kinds[1], RuleKind::Unevaluable);
        assert_eq!(kinds[2], RuleKind::Unevaluable);
        assert_eq!(kinds[3], RuleKind::Unevaluable);
    }

    #[test]
    fn the_send_decides_the_message_consent_leaf_of_every_rule() {
        let rules = [rule("a", json!({"v": 1, "consent": {"status": "none"}}))];
        let sql = |consent| match &compiled(&rules, consent)[0].kind {
            RuleKind::Query(query) => query.sql.clone(),
            other => panic!("{other:?}"),
        };

        assert!(sql(RecordedConsent::None).contains("AND (true)"));
        assert!(sql(RecordedConsent::Denied).contains("AND (false)"));
    }

    #[test]
    fn the_facts_hold_the_send_and_the_messages() {
        let facts = PolicyFacts {
            send: SendContext {
                channel: Channel::Email,
                dimensions: Dimensions::new(),
                consent: RecordedConsent::None,
            },
            messages: Vec::new(),
        };
        assert_eq!(facts.send.consent, RecordedConsent::None);
        assert!(facts.messages.is_empty());
    }
}
