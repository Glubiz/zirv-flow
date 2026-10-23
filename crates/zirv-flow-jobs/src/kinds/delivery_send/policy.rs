//! The consent policy at gate 3 (architecture 10.1, OQ-32): the published rules, compiled for the
//! send and evaluated for the person in order. The first rule that matches denies the send and is
//! the one the message names.

use time::OffsetDateTime;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::policy::{PolicyRule, RECORDED_CONSENT_RULE};
use zirv_flow_core::consent::{Channel, Dimensions, RecordedConsent};
use zirv_flow_segments::{Catalog, Definition, SendContext, TraitStorage, compile_policy};
use zirv_flow_store::{
    Clock, DecidingRule, PgPool, RuleCheck, RuleToCheck, begin, evaluate_consent_policy,
    published_consent_policy, segment_catalog,
};

use crate::JobError;
use crate::kinds::segment_refresh::{catalog_of, sql_query};

/// What decided a send that the policy denies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Denial {
    pub(super) id: String,
    pub(super) name: String,
}

impl From<DecidingRule> for Denial {
    fn from(rule: DecidingRule) -> Self {
        Self {
            id: rule.id,
            name: rule.name,
        }
    }
}

impl Denial {
    pub(super) fn is_builtin(&self) -> bool {
        self.id == RECORDED_CONSENT_RULE
    }
}

/// How each rule is checked. The built-in rule matches when the recorded consent denies the send.
/// A rule that no longer compiles, because something it reads was archived since it was published,
/// matches: a consent system stops the send rather than let it through.
pub(super) fn checks(
    rules: &[PolicyRule],
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
    send: &SendContext,
    recorded_denies: bool,
) -> Vec<RuleToCheck> {
    rules
        .iter()
        .map(|rule| {
            let check = match &rule.condition {
                None => RuleCheck::RecordedConsent {
                    denies: recorded_denies,
                },
                Some(condition) => Definition::from_value(condition)
                    .ok()
                    .and_then(|definition| {
                        compile_policy(&definition, catalog, storage, now, send).ok()
                    })
                    .map_or(RuleCheck::Unevaluable, |compiled| {
                        RuleCheck::Query(sql_query(compiled.batch_sql()))
                    }),
            };
            RuleToCheck {
                id: rule.id.clone(),
                name: rule.name.clone(),
                check,
            }
        })
        .collect()
}

/// The rule that denies the send, if one does. With no published policy only the built-in rule
/// exists, so the answer is the recorded consent's alone and nothing else is read.
pub(super) async fn denial(
    pool: &PgPool,
    clock: &Clock,
    workspace: WorkspaceId,
    person: uuid::Uuid,
    dimensions: &Dimensions,
    recorded: RecordedConsent,
    recorded_denies: bool,
) -> Result<Option<Denial>, JobError> {
    let builtin = || {
        Denial::from(DecidingRule {
            id: RECORDED_CONSENT_RULE.to_owned(),
            name: zirv_flow_core::consent::policy::RECORDED_CONSENT_NAME.to_owned(),
        })
    };
    let Some(published) = published_consent_policy(pool, workspace).await? else {
        return Ok(recorded_denies.then(builtin));
    };
    // A transaction of its own that ends here, as the other gates that read the workspace.
    let mut connection = begin(pool).await?;
    let now = clock.now(&mut *connection).await?;
    let send = SendContext {
        channel: Channel::Email,
        dimensions: dimensions.clone(),
        consent: recorded,
    };
    // The catalog is only read when a rule has a condition to compile.
    let (catalog, storage) = if published.rules.iter().any(|rule| rule.condition.is_some()) {
        catalog_of(segment_catalog(&mut *connection, workspace).await?)
    } else {
        (Catalog::default(), TraitStorage::default())
    };
    let rules = checks(
        &published.rules,
        &catalog,
        &storage,
        now,
        &send,
        recorded_denies,
    );
    Ok(
        evaluate_consent_policy(&mut *connection, workspace, person, &rules)
            .await?
            .map(Denial::from),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-01-01 12:00 UTC);

    fn send() -> SendContext {
        SendContext {
            channel: Channel::Email,
            dimensions: Dimensions::new(),
            consent: RecordedConsent::None,
        }
    }

    fn rule(n: u128, condition: serde_json::Value) -> PolicyRule {
        PolicyRule {
            id: Uuid::from_u128(n).to_string(),
            name: format!("Rule {n}"),
            condition: Some(condition),
        }
    }

    fn checked(rules: &[PolicyRule], denies: bool) -> Vec<RuleToCheck> {
        checks(
            rules,
            &Catalog::default(),
            &TraitStorage::default(),
            NOW,
            &send(),
            denies,
        )
    }

    #[test]
    fn the_rules_are_checked_in_their_own_order_with_the_built_in_one_where_it_was_put() {
        let rules = [
            rule(1, json!({"v": 1, "consent": {"status": "none"}})),
            PolicyRule::recorded_consent(),
            rule(2, json!({"v": 1, "consent": {"status": "denied"}})),
        ];

        let ids: Vec<String> = checked(&rules, true)
            .into_iter()
            .map(|rule| rule.id)
            .collect();

        assert_eq!(
            ids,
            [
                Uuid::from_u128(1).to_string(),
                RECORDED_CONSENT_RULE.to_owned(),
                Uuid::from_u128(2).to_string()
            ]
        );
    }

    #[test]
    fn the_built_in_rule_carries_what_the_recorded_consent_said() {
        let rules = [PolicyRule::recorded_consent()];

        assert_eq!(
            checked(&rules, true)[0].check,
            RuleCheck::RecordedConsent { denies: true }
        );
        assert_eq!(
            checked(&rules, false)[0].check,
            RuleCheck::RecordedConsent { denies: false }
        );
    }

    #[test]
    fn a_compiled_rule_is_a_query_for_one_person_and_one_that_does_not_compile_denies() {
        let good = rule(1, json!({"v": 1, "consent": {"status": "none"}}));
        let reads_an_archived_trait = rule(2, json!({"v": 1, "trait": "gone", "op": "exists"}));
        let not_a_definition = rule(3, json!({"nope": true}));

        let checks = checked(&[good, reads_an_archived_trait, not_a_definition], false);

        assert!(
            matches!(&checks[0].check, RuleCheck::Query(query) if query.sql.contains("p.id = ANY("))
        );
        assert_eq!(checks[1].check, RuleCheck::Unevaluable);
        assert_eq!(checks[2].check, RuleCheck::Unevaluable);
    }

    #[test]
    fn the_send_decides_the_message_consent_leaf_of_every_rule() {
        let rules = [rule(1, json!({"v": 1, "consent": {"status": "none"}}))];

        let at_none = checks(
            &rules,
            &Catalog::default(),
            &TraitStorage::default(),
            NOW,
            &send(),
            false,
        );
        let denied = SendContext {
            consent: RecordedConsent::Denied,
            ..send()
        };
        let at_denied = checks(
            &rules,
            &Catalog::default(),
            &TraitStorage::default(),
            NOW,
            &denied,
            true,
        );

        let sql = |checked: &[RuleToCheck]| match &checked[0].check {
            RuleCheck::Query(query) => query.sql.clone(),
            other => panic!("{other:?}"),
        };
        assert!(sql(&at_none).contains("AND (true)"));
        assert!(sql(&at_denied).contains("AND (false)"));
    }

    #[test]
    fn a_denial_is_the_built_in_one_by_its_id() {
        assert!(
            Denial {
                id: RECORDED_CONSENT_RULE.into(),
                name: "x".into()
            }
            .is_builtin()
        );
        assert!(
            !Denial {
                id: Uuid::nil().to_string(),
                name: "x".into()
            }
            .is_builtin()
        );
    }
}
