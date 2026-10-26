//! What a template can read: the person, the trigger event, the message and the system values
//! (architecture 11.4). Every map is ordered, so output never depends on insertion order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;

use crate::content::blocks::SnippetContent;
use crate::issue::FieldCatalog;
use crate::theme::ThemeTokens;

/// The unsubscribe URL of the sample context: a marketing email must link to exactly this.
pub const SAMPLE_UNSUBSCRIBE_URL: &str = "https://unsubscribe.sample.invalid/u";
/// The preference-centre URL of the sample context.
pub const SAMPLE_PREFERENCES_URL: &str = "https://preferences.sample.invalid/p";

/// The address a sample person has.
pub const SAMPLE_EMAIL: &str = "sample@example.invalid";
/// The reserved person field the send address arrives in (`person.email`). It is a string and
/// always known to a template, whatever the field registry holds.
pub const RESERVED_EMAIL: &str = "email";

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

    /// [`Self::sample`] with the sample person's address as `person.email`: what a preview of a
    /// sample person and a test send with no person render.
    pub fn sample_person(catalog: &FieldCatalog) -> Self {
        let mut context = Self::sample(catalog);
        context
            .person
            .insert(RESERVED_EMAIL.to_owned(), Value::from(SAMPLE_EMAIL));
        context
    }

    /// The context of a real person: the values of the traits the catalog knows (a trait the
    /// registry archived is not offered to templates), their own address as `person.email` when
    /// they have one, and sample event properties, because a preview or a test send has no
    /// triggering event. `traits` is the person's stored traits object.
    pub fn real_person(catalog: &FieldCatalog, traits: &Value, email: Option<String>) -> Self {
        let mut context = Self::sample(catalog);
        context.person.clear();
        for key in catalog.person.keys() {
            if let Some(value) = traits.get(key) {
                context.person.insert(key.clone(), value.clone());
            }
        }
        if let Some(email) = email {
            context
                .person
                .insert(RESERVED_EMAIL.to_owned(), Value::from(email));
        }
        context
    }
}

/// The inputs of a block render beyond the context, all resolved by the caller: the crate holds
/// no tables and loads nothing. Every field is optional and defaults, so an old request, and a
/// document that needs none of them, still render.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderExtras {
    /// The theme in effect.
    pub theme: Option<ThemeTokens>,
    /// Resolved snippet contents by snippet id.
    pub snippets: BTreeMap<String, SnippetContent>,
    /// The result of each `visible_if`, by the id of the section or block that holds it.
    pub conditions: BTreeMap<String, bool>,
    /// The URL of each asset by asset id.
    pub assets: BTreeMap<String, String>,
}

impl RenderExtras {
    /// The `https` URL of an asset, if the caller passed one.
    pub fn asset_url(&self, id: &str) -> Option<&str> {
        self.assets.get(id).map(String::as_str).filter(|url| {
            url.starts_with("https://")
                && !url.contains(|c: char| {
                    c.is_whitespace() || matches!(c, '{' | '}' | '<' | '>' | '"')
                })
        })
    }

    /// The URL of the theme logo: the theme must be present and name a logo with a URL.
    pub fn theme_logo_url(&self) -> Option<&str> {
        let logo = self.theme.as_ref()?.logo.as_deref()?;
        self.asset_url(logo)
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
    fn the_sample_person_has_the_sample_address() {
        let sample = RenderContext::sample_person(&FieldCatalog::default());
        assert_eq!(sample.person[RESERVED_EMAIL], Value::from(SAMPLE_EMAIL));
        assert_eq!(sample.message.id, "msg_sample");
    }

    #[test]
    fn a_real_person_gets_only_the_fields_the_catalog_knows() {
        let mut catalog = FieldCatalog::default().with_reserved_email();
        catalog.person.insert("plan".into(), TraitType::String);
        catalog.person.insert("age".into(), TraitType::Number);
        let traits = serde_json::json!({"plan": "pro", "secret": "x"});

        let context = RenderContext::real_person(&catalog, &traits, Some("ann@example.com".into()));

        assert_eq!(context.person["plan"], Value::from("pro"));
        assert_eq!(context.person["email"], Value::from("ann@example.com"));
        assert!(!context.person.contains_key("secret"));
        assert!(!context.person.contains_key("age"));
        let without = RenderContext::real_person(&catalog, &traits, None);
        assert!(!without.person.contains_key("email"));
        let not_an_object = RenderContext::real_person(&catalog, &Value::Null, None);
        assert!(not_an_object.person.is_empty());
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
