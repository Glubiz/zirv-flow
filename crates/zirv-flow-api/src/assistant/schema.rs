//! Tool parameters from the API types (architecture 13.2): the schema a tool offers a model is
//! the schema of the request type the API already validates, lowered into the small subset every
//! provider can express (`zirv_flow_ai::JsonSchema`). The tool is never given a hand-written
//! second description of the same shape.
//!
//! The lowering:
//! - resolves `$ref` through the type's component schemas;
//! - turns a `oneOf` of null and X, and `type: [T, "null"]`, into X and T;
//! - turns a tagged `oneOf` of objects, each with a one-value string enum for the tag, into one
//!   object whose tag lists every variant, with the properties merged and only the tag required;
//! - drops `format`, `additionalProperties`, `default`, `example`, `minimum`, `maximum`, `title`
//!   and every other keyword the subset does not have.
//!
//! The merged object is more permissive than the union. The model's output is still checked by
//! the real validator, which is the one that decides.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use utoipa::openapi::{RefOr, Schema};
use utoipa::{PartialSchema, ToSchema};
use zirv_flow_ai::{AiError, JsonSchema};

/// How deep a schema may nest before it is refused: a guard against a cyclic `$ref`.
const MAX_DEPTH: usize = 12;

/// The tool-parameter schema of `T`.
pub(crate) fn tool_parameters<T: ToSchema>() -> Result<JsonSchema, AiError> {
    let mut components: Vec<(String, RefOr<Schema>)> =
        vec![(T::name().to_string(), <T as PartialSchema>::schema())];
    T::schemas(&mut components);
    let mut registry = BTreeMap::new();
    for (name, schema) in components {
        let value = serde_json::to_value(&schema)
            .map_err(|_| AiError::Schema("a type's schema cannot be read".to_owned()))?;
        registry.entry(name).or_insert(value);
    }
    let root = registry
        .get(T::name().as_ref())
        .cloned()
        .ok_or_else(|| AiError::Schema("a type's schema is missing".to_owned()))?;
    JsonSchema::from_value(lower(&root, &registry, 0)?)
}

fn unsupported(reason: &str) -> AiError {
    AiError::Schema(reason.to_owned())
}

/// The lowered form of one schema node.
fn lower(node: &Value, registry: &BTreeMap<String, Value>, depth: usize) -> Result<Value, AiError> {
    if depth > MAX_DEPTH {
        return Err(unsupported("nested too deeply"));
    }
    let Some(map) = node.as_object() else {
        return Err(unsupported("a schema must be an object"));
    };
    let description = map.get("description").and_then(Value::as_str);
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        let target = registry
            .get(name)
            .ok_or_else(|| unsupported("a $ref names a missing schema"))?;
        return with_description(lower(target, registry, depth + 1)?, description);
    }
    for combinator in ["allOf", "oneOf", "anyOf"] {
        if let Some(Value::Array(variants)) = map.get(combinator) {
            let lowered = lower_variants(variants, registry, depth)?;
            return with_description(lowered, description);
        }
    }
    if let Some(constant) = map.get("const") {
        return with_description(json!({"type": "string", "enum": [constant]}), description);
    }

    let kind = match map.get("type") {
        Some(Value::String(kind)) => Some(kind.as_str()),
        // `[T, "null"]` is T.
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ => None,
    };
    let kind = match (kind, map.contains_key("properties")) {
        (Some(kind), _) => kind,
        (None, true) => "object",
        (None, false) if map.contains_key("enum") => "string",
        // A schema with no type is any JSON value; the subset has no such type, so it is an
        // object a caller fills freely.
        (None, false) => "object",
    };
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    if let Some(values) = map.get("enum") {
        out.insert("enum".into(), values.clone());
    }
    if let Some(text) = description {
        out.insert("description".into(), json!(text));
    }
    match kind {
        "object" => {
            let mut properties = Map::new();
            if let Some(Value::Object(declared)) = map.get("properties") {
                for (name, child) in declared {
                    properties.insert(name.clone(), lower(child, registry, depth + 1)?);
                }
            }
            if !properties.is_empty() {
                let required: Vec<&Value> = map
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter(|name| {
                                name.as_str()
                                    .is_some_and(|name| properties.contains_key(name))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !required.is_empty() {
                    out.insert("required".into(), json!(required));
                }
                out.insert("properties".into(), Value::Object(properties));
            }
        }
        "array" => {
            let items = map
                .get("items")
                .ok_or_else(|| unsupported("an array needs items"))?;
            out.insert("items".into(), lower(items, registry, depth + 1)?);
        }
        _ => {}
    }
    Ok(Value::Object(out))
}

/// Keeps the description of the node that referred to a schema, over the target's.
fn with_description(mut lowered: Value, description: Option<&str>) -> Result<Value, AiError> {
    if let (Some(text), Some(map)) = (description, lowered.as_object_mut()) {
        map.insert("description".into(), json!(text));
    }
    Ok(lowered)
}

fn is_null(variant: &Value) -> bool {
    variant.get("type").and_then(Value::as_str) == Some("null")
}

/// A `oneOf`, `anyOf` or `allOf`: a nullable wrapper, a union of tagged objects or of one-value
/// strings.
fn lower_variants(
    variants: &[Value],
    registry: &BTreeMap<String, Value>,
    depth: usize,
) -> Result<Value, AiError> {
    let present: Vec<&Value> = variants
        .iter()
        .filter(|variant| !is_null(variant))
        .collect();
    let lowered = present
        .iter()
        .map(|variant| lower(variant, registry, depth + 1))
        .collect::<Result<Vec<_>, _>>()?;
    match lowered.as_slice() {
        [] => Err(unsupported("a union has no variant")),
        [only] => Ok(only.clone()),
        _ => merge_union(&lowered),
    }
}

/// One-value string enums, or tagged objects, as a single schema.
fn merge_union(variants: &[Value]) -> Result<Value, AiError> {
    if let Some(tags) = variants.iter().map(single_tag).collect::<Option<Vec<_>>>() {
        return Ok(json!({"type": "string", "enum": tags}));
    }
    let all_objects = variants
        .iter()
        .all(|variant| variant.get("type").and_then(Value::as_str) == Some("object"));
    if !all_objects {
        return Err(unsupported("a union of mixed types is not supported"));
    }
    let property = |variant: &Value, name: &str| -> Option<Value> {
        variant.get("properties")?.get(name).cloned()
    };
    let first = variants[0]
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| unsupported("a union variant has no properties"))?;
    // The tag is the property every variant has, with one string value in each.
    let tag = first
        .keys()
        .find(|name| {
            variants.iter().all(|variant| {
                property(variant, name)
                    .as_ref()
                    .and_then(single_tag)
                    .is_some()
            })
        })
        .cloned()
        .ok_or_else(|| unsupported("a union of objects needs a tag in every variant"))?;
    let mut tags = Vec::new();
    let mut merged = Map::new();
    for variant in variants {
        let properties = variant
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| unsupported("a union variant has no properties"))?;
        for (name, schema) in properties {
            if *name == tag {
                tags.extend(single_tag(schema));
            } else {
                merged.entry(name.clone()).or_insert_with(|| schema.clone());
            }
        }
    }
    let mut properties = Map::new();
    properties.insert(tag.clone(), json!({"type": "string", "enum": tags}));
    properties.extend(merged);
    Ok(json!({"type": "object", "properties": properties, "required": [tag]}))
}

/// The one value of a string enum with exactly one value.
fn single_tag(schema: &Value) -> Option<Value> {
    let values = schema.get("enum")?.as_array()?;
    match values.as_slice() {
        [tag] if tag.is_string() => Some(tag.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use uuid::Uuid;

    use super::*;
    use crate::imports::MappingBody;

    fn property<'a>(schema: &'a JsonSchema, path: &[&str]) -> &'a Value {
        path.iter().fold(schema.as_value(), |node, key| match *key {
            "[]" => &node["items"],
            key => &node["properties"][key],
        })
    }

    #[test]
    fn the_mapping_body_lowers_into_the_subset() {
        let schema = tool_parameters::<MappingBody>().unwrap();

        assert_eq!(schema.as_value()["type"], "object");
        // `from_value` already refused every keyword outside the subset.
        let rule = property(&schema, &["columns", "[]"]);
        assert_eq!(rule["type"], "object");
        assert_eq!(rule["required"], json!(["column", "target"]));
        assert!(
            rule["properties"]["column"]["description"]
                .as_str()
                .unwrap()
                .contains("dotted path")
        );
        assert_eq!(
            property(&schema, &["columns", "[]", "transforms", "[]", "op"])["type"],
            "string"
        );
        // The event is optional and holds property rules.
        assert!(
            schema.as_value()["required"]
                .as_array()
                .unwrap()
                .iter()
                .all(|name| name != "event")
        );
        assert_eq!(
            property(&schema, &["event", "properties", "[]", "property"])["type"],
            "string"
        );
        // A map of strings keeps only its type.
        assert_eq!(
            property(&schema, &["columns", "[]", "transforms", "[]", "map"])["type"],
            "object"
        );
    }

    #[test]
    fn a_tagged_union_becomes_one_object_with_every_tag() {
        let schema = tool_parameters::<MappingBody>().unwrap();

        let target = property(&schema, &["columns", "[]", "target"]);
        assert_eq!(target["required"], json!(["type"]));
        assert_eq!(
            target["properties"]["type"]["enum"],
            json!(["identifier", "trait", "list", "ignore"])
        );
        for merged in ["kind", "key", "list_id"] {
            assert!(target["properties"][merged].is_object(), "{merged}");
        }
        let op = property(&schema, &["columns", "[]", "transforms", "[]", "op"]);
        let tags: Vec<&str> = op["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for expected in [
            "trim",
            "lower",
            "upper",
            "cast",
            "timestamp",
            "concat",
            "constant",
            "value_map",
            "split",
        ] {
            assert!(tags.contains(&expected), "{expected} in {tags:?}");
        }
        // The `to` of a cast is an enum resolved through its `$ref`.
        let cast_to = property(&schema, &["columns", "[]", "transforms", "[]", "to"]);
        assert!(
            cast_to["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("string_list"))
        );
    }

    #[test]
    fn nullable_refs_and_formats_are_dropped() {
        #[derive(Deserialize, ToSchema)]
        #[allow(dead_code)]
        struct Inner {
            /// The inner name.
            name: String,
        }
        #[derive(Deserialize, ToSchema)]
        #[allow(dead_code)]
        struct Outer {
            inner: Option<Inner>,
            id: Uuid,
            #[schema(minimum = 1, maximum = 5, default = 2)]
            count: Option<u32>,
            note: Option<String>,
        }

        let schema = tool_parameters::<Outer>().unwrap();

        let value = schema.as_value();
        assert_eq!(value["properties"]["inner"]["type"], "object");
        assert_eq!(
            value["properties"]["inner"]["properties"]["name"]["description"],
            "The inner name."
        );
        assert_eq!(value["properties"]["id"], json!({"type": "string"}));
        assert_eq!(value["properties"]["count"]["type"], "integer");
        assert_eq!(value["properties"]["note"], json!({"type": "string"}));
        let rendered = value.to_string();
        for dropped in ["format", "minimum", "maximum", "default", "null", "$ref"] {
            assert!(!rendered.contains(dropped), "{dropped} in {rendered}");
        }
        assert_eq!(
            value["required"],
            json!(["id"]),
            "only the non-optional fields are required"
        );
    }

    #[test]
    fn a_union_that_cannot_be_merged_is_refused() {
        let registry = BTreeMap::new();
        let mixed = json!({"oneOf": [{"type": "string"}, {"type": "object"}]});

        assert!(matches!(
            lower(&mixed, &registry, 0),
            Err(AiError::Schema(_))
        ));
        let cyclic = BTreeMap::from([("A".to_owned(), json!({"$ref": "#/components/schemas/A"}))]);
        assert!(matches!(
            lower(&json!({"$ref": "#/components/schemas/A"}), &cyclic, 0),
            Err(AiError::Schema(_))
        ));
    }

    /// A mapping the sources guide shows in its mapping reference.
    #[test]
    fn the_sources_guide_mapping_example_matches_the_lowered_schema() {
        let guide = include_str!("../../../../docs/guide/sources.md");
        let section = guide
            .find("Example, a JSON file of customers with an order each")
            .expect("the mapping example");
        let start = section + guide[section..].find("\"columns\"").unwrap();
        let open = guide[..start].rfind('{').unwrap();
        let mut depth = 0;
        let mut end = open;
        for (index, character) in guide[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let text = guide[open..end].replace("\"...\"", "\"0190d0f2-0000-7000-8000-000000000001\"");
        let example: Value = serde_json::from_str(&text).unwrap();
        let schema = tool_parameters::<MappingBody>().unwrap();

        assert_eq!(schema.validate_value(&example), Ok(()));
        // The real type reads it too, so the schema and the type describe one thing.
        assert!(serde_json::from_value::<MappingBody>(example).is_ok());
    }
}
