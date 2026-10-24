//! The rules of a consent policy (architecture 10.1, OQ-32): an ordered list of named deny-only
//! rules. A rule's condition is a definition of the segment AST; this module checks only the
//! shape of the list. The conditions are validated by `zirv-flow-segments`, which owns the AST.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The id of the built-in rule that matches when the recorded consent does not allow the send.
pub const RECORDED_CONSENT_RULE: &str = "recorded_consent";
/// The name of the built-in rule.
pub const RECORDED_CONSENT_NAME: &str = "Recorded consent";
/// The most rules a policy holds, the built-in one included.
pub const MAX_RULES: usize = 50;
/// The longest rule name, in characters.
pub const MAX_NAME_CHARS: usize = 100;

/// One rule. The built-in rule has no condition; every other rule has one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyRule {
    /// `recorded_consent` for the built-in rule; otherwise a UUID, which the gate result and the
    /// decision name the rule by.
    pub id: String,
    pub name: String,
    /// A segment definition (`{"v": 1, ...}`), valid in a policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Value>,
}

impl PolicyRule {
    /// The built-in rule.
    pub fn recorded_consent() -> Self {
        Self {
            id: RECORDED_CONSENT_RULE.to_owned(),
            name: RECORDED_CONSENT_NAME.to_owned(),
            condition: None,
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.id == RECORDED_CONSENT_RULE
    }
}

/// The rules of a workspace that has defined none: only the built-in rule, which is the topic
/// default and the recorded consent.
pub fn default_rules() -> Vec<PolicyRule> {
    vec![PolicyRule::recorded_consent()]
}

/// Why a list of rules is refused. Each names the rule by its position, never a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RulesError {
    TooMany,
    /// The built-in rule is not in the list: it can be moved but not removed.
    BuiltinMissing,
    /// An id is neither `recorded_consent` nor a UUID.
    BadId(usize),
    DuplicateId(usize),
    /// A name is blank or longer than [`MAX_NAME_CHARS`].
    BadName(usize),
    /// The built-in rule carries a condition.
    BuiltinHasCondition(usize),
    /// A rule other than the built-in one has no condition.
    ConditionMissing(usize),
}

impl RulesError {
    /// A fixed code for the problem body.
    pub const fn code(self) -> &'static str {
        match self {
            Self::TooMany => "too_many_rules",
            Self::BuiltinMissing => "builtin_rule_required",
            Self::BadId(_) => "invalid_rule_id",
            Self::DuplicateId(_) => "duplicate_rule_id",
            Self::BadName(_) => "invalid_rule_name",
            Self::BuiltinHasCondition(_) => "builtin_rule_has_condition",
            Self::ConditionMissing(_) => "rule_condition_required",
        }
    }

    /// The position of the rule concerned, when one is.
    pub const fn index(self) -> Option<usize> {
        match self {
            Self::TooMany | Self::BuiltinMissing => None,
            Self::BadId(index)
            | Self::DuplicateId(index)
            | Self::BadName(index)
            | Self::BuiltinHasCondition(index)
            | Self::ConditionMissing(index) => Some(index),
        }
    }
}

fn is_rule_id(text: &str) -> bool {
    text == RECORDED_CONSENT_RULE || Uuid::parse_str(text).is_ok()
}

/// Checks the shape of a list of rules: the built-in rule is present once (anywhere in the
/// order), the ids are unique UUIDs (or the built-in id), the names are 1 to 100 characters, and every other rule has a
/// condition. The conditions themselves are not looked at.
pub fn check_rules(rules: &[PolicyRule]) -> Result<(), RulesError> {
    if rules.len() > MAX_RULES {
        return Err(RulesError::TooMany);
    }
    let mut seen = BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        if !is_rule_id(&rule.id) {
            return Err(RulesError::BadId(index));
        }
        if !seen.insert(rule.id.as_str()) {
            return Err(RulesError::DuplicateId(index));
        }
        let length = rule.name.chars().count();
        if rule.name.trim().is_empty() || length > MAX_NAME_CHARS {
            return Err(RulesError::BadName(index));
        }
        match (rule.is_builtin(), &rule.condition) {
            (true, Some(_)) => return Err(RulesError::BuiltinHasCondition(index)),
            (false, None) => return Err(RulesError::ConditionMissing(index)),
            _ => {}
        }
    }
    if !seen.contains(RECORDED_CONSENT_RULE) {
        return Err(RulesError::BuiltinMissing);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id_of(n: u128) -> String {
        Uuid::from_u128(n).to_string()
    }

    fn custom(id: &str) -> PolicyRule {
        PolicyRule {
            id: id.to_owned(),
            name: format!("Rule {id}"),
            condition: Some(json!({"v": 1, "consent": {"status": "none"}})),
        }
    }

    #[test]
    fn the_default_policy_is_the_built_in_rule_alone() {
        assert_eq!(check_rules(&default_rules()), Ok(()));
        assert!(default_rules()[0].is_builtin());
    }

    #[test]
    fn the_built_in_rule_can_be_moved_anywhere_but_not_removed() {
        let moved = [
            custom(&id_of(1)),
            PolicyRule::recorded_consent(),
            custom(&id_of(2)),
        ];
        assert_eq!(check_rules(&moved), Ok(()));
        assert_eq!(
            check_rules(&[custom(&id_of(1))]),
            Err(RulesError::BuiltinMissing)
        );
        assert_eq!(check_rules(&[]), Err(RulesError::BuiltinMissing));
    }

    #[test]
    fn ids_names_and_conditions_are_checked_by_position() {
        let builtin = PolicyRule::recorded_consent();
        assert_eq!(
            check_rules(&[builtin.clone(), custom("Bad Id")]),
            Err(RulesError::BadId(1))
        );
        assert_eq!(
            check_rules(&[builtin.clone(), custom(&id_of(1)), custom(&id_of(1))]),
            Err(RulesError::DuplicateId(2))
        );
        let mut blank = custom(&id_of(1));
        blank.name = "  ".into();
        assert_eq!(
            check_rules(&[builtin.clone(), blank]),
            Err(RulesError::BadName(1))
        );
        let mut long = custom(&id_of(1));
        long.name = "n".repeat(101);
        assert_eq!(
            check_rules(&[builtin.clone(), long]),
            Err(RulesError::BadName(1))
        );
        let mut bare = custom(&id_of(1));
        bare.condition = None;
        assert_eq!(
            check_rules(&[builtin.clone(), bare]),
            Err(RulesError::ConditionMissing(1))
        );
        let mut conditioned = builtin;
        conditioned.condition = Some(json!({}));
        assert_eq!(
            check_rules(&[conditioned]),
            Err(RulesError::BuiltinHasCondition(0))
        );
    }

    #[test]
    fn a_policy_holds_at_most_fifty_rules() {
        let mut rules = vec![PolicyRule::recorded_consent()];
        rules.extend((0..MAX_RULES - 1).map(|n| custom(&id_of(100 + n as u128))));
        assert_eq!(check_rules(&rules), Ok(()));
        rules.push(custom(&id_of(999)));
        assert_eq!(check_rules(&rules), Err(RulesError::TooMany));
    }

    #[test]
    fn a_rule_serializes_without_a_missing_condition() {
        assert_eq!(
            serde_json::to_value(PolicyRule::recorded_consent()).unwrap(),
            json!({"id": "recorded_consent", "name": "Recorded consent"})
        );
        let parsed: PolicyRule =
            serde_json::from_value(json!({"id": "a", "name": "A", "condition": {"v": 1}})).unwrap();
        assert_eq!(parsed.condition, Some(json!({"v": 1})));
    }
}
