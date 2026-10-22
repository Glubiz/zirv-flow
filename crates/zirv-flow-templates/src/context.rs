//! What a template can read: the person, the trigger event, the message and the system values
//! (architecture 11.4). Every map is ordered, so output never depends on insertion order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
