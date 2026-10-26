//! The pure migrations of a stored template document (architecture 11.8).
//!
//! A document is a JSON value until it is read, so a migration is a function from one schema's
//! value to the next one's. Published versions are stored as they were saved and migrate on every
//! read; a draft is saved at the current schema. Nothing here reads the typed model, so an old
//! document never has to fit the current types.

use serde_json::{Value, json};

use crate::content::blocks::BlocksError;
use crate::document::SCHEMA_VERSION;

/// Schema 1 to schema 2. A block email gets `theme: "none"` and `footer: "none"`, so it renders
/// from its own `settings` exactly as before, and each image `src` string becomes
/// `{"url": <string>}`. Every other document changes in its `schema` alone. A value that is not
/// a schema 1 document is returned as it is.
pub fn v1_to_v2(mut value: Value) -> Value {
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return value;
    }
    if let Some(blocks) = value.get_mut("blocks").and_then(Value::as_object_mut) {
        blocks.entry("theme").or_insert_with(|| json!("none"));
        blocks.entry("footer").or_insert_with(|| json!("none"));
        let sections = blocks.get_mut("sections").and_then(Value::as_array_mut);
        for section in sections.into_iter().flatten() {
            let columns = section.get_mut("columns").and_then(Value::as_array_mut);
            for column in columns
                .into_iter()
                .flatten()
                .filter_map(Value::as_array_mut)
            {
                for block in column {
                    migrate_image(block);
                }
            }
        }
    }
    value["schema"] = json!(2);
    value
}

fn migrate_image(block: &mut Value) {
    if block.get("type").and_then(Value::as_str) != Some("image") {
        return;
    }
    if let Some(src) = block.get_mut("src")
        && src.is_string()
    {
        *src = json!({ "url": src.take() });
    }
}

/// Bring a stored document to [`SCHEMA_VERSION`]. A document without a `schema` is
/// [`BlocksError::Malformed`]; one at a schema this crate does not know (a newer one) is
/// [`BlocksError::UnsupportedSchema`]. A document at the current schema is returned unchanged.
pub fn migrate_to_current(value: Value) -> Result<Value, BlocksError> {
    let Some(schema) = value.get("schema") else {
        return Err(BlocksError::Malformed);
    };
    match schema.as_u64() {
        Some(1) => Ok(v1_to_v2(value)),
        Some(schema) if schema == u64::from(SCHEMA_VERSION) => Ok(value),
        _ => Err(BlocksError::UnsupportedSchema),
    }
}

/// A stored fixture read the way every stored document is: migrated, then typed.
#[cfg(test)]
pub(crate) fn read_doc(json: &str) -> crate::document::TemplateDocument {
    let value: Value = serde_json::from_str(json).expect("fixture is JSON");
    crate::content::blocks::parse_template_document(&value).expect("fixture is a template")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn v1() -> Value {
        json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
        "subject": "Hi",
        "sections": [{"id": "s", "columns": [[
            {"type": "image", "id": "i", "src": "https://img.example/a.png", "alt": "a"},
            {"type": "heading", "id": "h", "level": 1, "text": "Hi"}
        ]]}]}})
    }

    #[test]
    fn a_block_email_gets_no_theme_no_footer_and_a_url_src() {
        let migrated = v1_to_v2(v1());
        assert_eq!(migrated["schema"], 2);
        assert_eq!(migrated["blocks"]["theme"], "none");
        assert_eq!(migrated["blocks"]["footer"], "none");
        let image = &migrated["blocks"]["sections"][0]["columns"][0][0];
        assert_eq!(image["src"], json!({"url": "https://img.example/a.png"}));
        assert_eq!(
            migrated["blocks"]["sections"][0]["columns"][0][1]["text"],
            "Hi"
        );
    }

    #[test]
    fn a_code_document_changes_in_its_schema_alone() {
        let code = json!({"schema": 1, "channel": "email", "mode": "code",
                          "email": {"subject": "s", "html": "<p>x</p>"}});
        let migrated = v1_to_v2(code.clone());
        assert_eq!(migrated["schema"], 2);
        assert_eq!(migrated["email"], code["email"]);
        assert!(migrated.get("blocks").is_none());
    }

    #[test]
    fn only_a_schema_one_document_is_migrated_and_a_newer_one_is_refused() {
        assert_eq!(migrate_to_current(v1()).unwrap(), v1_to_v2(v1()));
        for schema in [json!(3), json!(0), json!("1"), json!(null)] {
            let mut doc = v1();
            doc["schema"] = schema;
            assert_eq!(
                migrate_to_current(doc).unwrap_err(),
                BlocksError::UnsupportedSchema
            );
        }
        assert_eq!(
            migrate_to_current(json!({"channel": "email"})).unwrap_err(),
            BlocksError::Malformed
        );
    }

    proptest! {
        #[test]
        fn migrating_is_idempotent_and_leaves_schema_two_alone(
            subject in ".{0,40}",
            url in r"https://[a-z]{1,10}\.example/[a-z]{0,10}",
            blocks in 0usize..4,
        ) {
            let images: Vec<Value> = (0..blocks)
                .map(|n| json!({"type": "image", "id": format!("i{n}"), "src": url}))
                .collect();
            let doc = json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
                "subject": subject, "sections": [{"id": "s", "columns": [images]}]}});
            let once = migrate_to_current(doc).unwrap();
            let twice = migrate_to_current(once.clone()).unwrap();
            prop_assert_eq!(&once, &twice);
            prop_assert_eq!(once["schema"].as_u64(), Some(u64::from(SCHEMA_VERSION)));
        }
    }
}
