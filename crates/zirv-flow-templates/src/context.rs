//! What a template can read: the person, the trigger event, the message and the system values
//! (architecture 11.4). Every map is ordered, so output never depends on insertion order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;

use crate::issue::FieldCatalog;

/// The unsubscribe URL of the sample context: a marketing email must link to exactly this.
pub const SAMPLE_UNSUBSCRIBE_URL: &str = "https://unsubscribe.sample.invalid/u";
/// The preference-centre URL of the sample context.
pub const SAMPLE_PREFERENCES_URL: &str = "https://preferences.sample.invalid/p";

/// The values behind `person`, `event`, `message` and `system` in a template.
///
/// Person traits and event properties carry their registry type (string, number, boolean, list):
/// a webhook value that is exactly one merge tag keeps that type.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct RenderContext {
    /// Trait key to value.
    pub person: BTreeMap<String, Value>,
    /// Trigger event property name to value.
    pub event: BTreeMap<String, Value>,
    pub message: MessageContext,
    pub system: SystemValues,
}

impl RenderContext {
    /// A deterministic sample: one value for every field in the catalog, by type, the message id
    /// `msg_sample` and sentinel system URLs. Validation renders against it, and preview uses it
    /// for a sample person, so both see the same values.
    pub fn sample(catalog: &FieldCatalog) -> Self {
        let mut context = Self::default();
        for (key, kind) in &catalog.person {
            let value = match kind {
                TraitType::String => serde_json::json!("sample"),
                TraitType::Number => serde_json::json!(1),
                TraitType::Boolean => serde_json::json!(true),
                TraitType::Timestamp => serde_json::json!("2026-01-01T00:00:00Z"),
                TraitType::Date => serde_json::json!("2026-01-01"),
                TraitType::StringList => serde_json::json!(["sample"]),
            };
            context.person.insert(key.clone(), value);
        }
        for (key, kind) in &catalog.event {
            let value = match kind {
                PropertyType::String => serde_json::json!("sample"),
                PropertyType::Number | PropertyType::Integer => serde_json::json!(1),
                PropertyType::Boolean => serde_json::json!(true),
                PropertyType::Timestamp => serde_json::json!("2026-01-01T00:00:00Z"),
                PropertyType::Date => serde_json::json!("2026-01-01"),
                PropertyType::StringList => serde_json::json!(["sample"]),
                PropertyType::Object => serde_json::json!({}),
            };
            context.event.insert(key.clone(), value);
        }
        context.message.id = "msg_sample".to_owned();
        context.system.unsubscribe_url = SAMPLE_UNSUBSCRIBE_URL.to_owned();
        context.system.preferences_url = SAMPLE_PREFERENCES_URL.to_owned();
        context
    }
}

/// The message being rendered.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct MessageContext {
    pub id: String,
}

/// System values the caller computes (and signs); the crate only places them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SystemValues {
    /// The message's signed unsubscribe URL; also the target of `List-Unsubscribe`.
    pub unsubscribe_url: String,
    /// The message's signed preference-centre URL.
    pub preferences_url: String,
}

/// The fixed fields of `message`.
pub(crate) const MESSAGE_FIELDS: [&str; 1] = ["id"];
/// The fixed fields of `system`.
pub(crate) const SYSTEM_FIELDS: [&str; 2] = ["unsubscribe_url", "preferences_url"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sections_default() {
        let context: RenderContext = serde_json::from_str(r#"{"person":{"a":1}}"#).unwrap();
        assert_eq!(context.person["a"], Value::from(1));
        assert!(context.event.is_empty());
        assert_eq!(context.system.unsubscribe_url, "");
    }

    #[test]
    fn the_sample_has_a_value_for_every_catalog_field_and_is_stable() {
        let mut catalog = FieldCatalog::default();
        catalog.person.insert("plan".into(), TraitType::String);
        catalog.person.insert("age".into(), TraitType::Number);
        catalog.event.insert("total".into(), PropertyType::Number);
        let sample = RenderContext::sample(&catalog);
        assert_eq!(sample.person["plan"], Value::from("sample"));
        assert_eq!(sample.person["age"], Value::from(1));
        assert_eq!(sample.event["total"], Value::from(1));
        assert_eq!(sample.message.id, "msg_sample");
        assert_eq!(sample.system.unsubscribe_url, SAMPLE_UNSUBSCRIBE_URL);
        assert_eq!(sample, RenderContext::sample(&catalog));
    }

    #[test]
    fn serialisation_is_in_key_order() {
        let mut one = RenderContext::default();
        one.person.insert("b".into(), Value::from(1));
        one.person.insert("a".into(), Value::from(2));
        let mut two = RenderContext::default();
        two.person.insert("a".into(), Value::from(2));
        two.person.insert("b".into(), Value::from(1));
        assert_eq!(
            serde_json::to_string(&one).unwrap(),
            serde_json::to_string(&two).unwrap()
        );
    }
}
