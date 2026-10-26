//! A condition stored outside a segment: a flow branch case or a block's `visible_if`. It is the
//! segment wire form without its version key, so it is parsed, validated and compiled like a
//! segment definition (architecture 8.2 and 11.4).

use serde_json::Value;
use time::OffsetDateTime;
use zirv_flow_core::segment::{Definition, Issue, code};

use crate::catalog::Catalog;
use crate::compile::{CompiledSegment, compile};
use crate::storage::TraitStorage;
use crate::validate::parse_and_validate;

/// Why a condition is not usable: the issues of its parse, validation or compile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionError {
    pub issues: Vec<Issue>,
}

impl ConditionError {
    /// Whether the condition names a segment the workspace does not have: a reference that broke,
    /// rather than a condition that was never valid.
    pub fn is_broken_segment(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.code == code::UNKNOWN_SEGMENT)
    }
}

/// Parses `condition` and validates it against `catalog`.
pub fn parse_condition(condition: &Value, catalog: &Catalog) -> Result<Definition, ConditionError> {
    let Some(object) = condition.as_object() else {
        return Err(ConditionError {
            issues: vec![Issue::new("", code::MALFORMED)],
        });
    };
    let mut wire = object.clone();
    wire.insert("v".to_owned(), 1.into());
    parse_and_validate(&Value::Object(wire), catalog).map_err(|issues| ConditionError { issues })
}

/// [`parse_condition`], then lowers the condition to SQL at the explicit time `now`.
pub fn compile_condition(
    condition: &Value,
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
) -> Result<CompiledSegment, ConditionError> {
    let definition = parse_condition(condition, catalog)?;
    compile(&definition, catalog, storage, now).map_err(|issues| ConditionError { issues })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::people::TraitType;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog
    }

    fn plan_is(value: &str) -> Value {
        json!({"trait": "plan", "op": "eq", "value": value})
    }

    #[test]
    fn a_valid_condition_compiles_with_or_without_the_version_key() {
        let storage = TraitStorage::default();
        let bare = compile_condition(&plan_is("pro"), &catalog(), &storage, NOW).unwrap();
        let mut tagged = plan_is("pro");
        tagged["v"] = json!(1);
        let same = compile_condition(&tagged, &catalog(), &storage, NOW).unwrap();
        assert_eq!(bare, same);
    }

    #[test]
    fn a_condition_that_is_not_an_object_is_malformed() {
        let error = parse_condition(&json!("pro"), &catalog()).unwrap_err();
        assert_eq!(error.issues, [Issue::new("", code::MALFORMED)]);
        assert!(!error.is_broken_segment());
    }

    #[test]
    fn an_unknown_trait_is_invalid_and_an_unknown_segment_is_a_broken_reference() {
        let unknown_trait = json!({"trait": "nope", "op": "eq", "value": "x"});
        let invalid = parse_condition(&unknown_trait, &catalog()).unwrap_err();
        assert!(!invalid.is_broken_segment());
        assert_eq!(invalid.issues[0].code, code::UNKNOWN_TRAIT);

        let missing = json!({"segment": Uuid::now_v7().to_string()});
        let broken = parse_condition(&missing, &catalog()).unwrap_err();
        assert!(broken.is_broken_segment());
    }
}
