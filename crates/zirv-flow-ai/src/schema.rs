//! The tool-parameter schema subset every supported provider can express (architecture 13.5):
//! object, string, number, integer, boolean, array, enum and required. Anything else is refused
//! when the schema is built, so no provider ever sees a keyword it may ignore or reject.

use serde_json::{Map, Value};

use crate::error::AiError;

const KEYWORDS: [&str; 6] = [
    "type",
    "description",
    "properties",
    "required",
    "items",
    "enum",
];
const TYPES: [&str; 6] = ["object", "string", "number", "integer", "boolean", "array"];
const MAX_DEPTH: usize = 8;

/// A JSON Schema document inside the supported subset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonSchema(Value);

impl JsonSchema {
    /// Builds a schema from JSON, refusing anything outside the subset. The root must be an
    /// object schema, because that is what a tool's parameters are.
    pub fn from_value(value: Value) -> Result<Self, AiError> {
        let schema = Self(value);
        schema.validate_subset()?;
        Ok(schema)
    }

    /// Checks the schema against the subset, naming the first offending path.
    pub fn validate_subset(&self) -> Result<(), AiError> {
        if self.0.get("type").and_then(Value::as_str) != Some("object") {
            return Err(AiError::Schema("$: the root must be an object".to_owned()));
        }
        check(&self.0, "$", 0).map_err(AiError::Schema)
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Checks a value (a tool call's arguments) against the schema, listing every violation.
    pub fn validate_value(&self, value: &Value) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        validate(&self.0, value, "$", &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn check(node: &Value, path: &str, depth: usize) -> Result<(), String> {
    let fail = |reason: &str| Err(format!("{path}: {reason}"));
    if depth > MAX_DEPTH {
        return fail("nested too deeply");
    }
    let Some(map) = node.as_object() else {
        return fail("a schema must be an object");
    };
    if let Some(keyword) = map.keys().find(|key| !KEYWORDS.contains(&key.as_str())) {
        return Err(format!("{path}: the keyword {keyword:?} is not supported"));
    }
    let has_enum = map.contains_key("enum");
    let kind = match map.get("type") {
        None if has_enum => "string",
        None => return fail("a type is required"),
        Some(Value::String(kind)) if TYPES.contains(&kind.as_str()) => kind.as_str(),
        Some(_) => return fail("the type is not supported"),
    };
    if let Some(values) = map.get("enum") {
        let strings = values
            .as_array()
            .is_some_and(|list| !list.is_empty() && list.iter().all(Value::is_string));
        if kind != "string" || !strings {
            return fail("an enum is a non-empty list of strings");
        }
    }
    if map.contains_key("properties") && kind != "object" {
        return fail("properties belong to an object");
    }
    if map.contains_key("required") && kind != "object" {
        return fail("required belongs to an object");
    }
    if map.contains_key("items") && kind != "array" {
        return fail("items belong to an array");
    }
    if let Some(description) = map.get("description")
        && !description.is_string()
    {
        return fail("the description must be text");
    }
    if kind == "object" {
        let empty = Map::new();
        let properties = match map.get("properties") {
            None => &empty,
            Some(Value::Object(properties)) => properties,
            Some(_) => return fail("properties must be an object"),
        };
        for (name, child) in properties {
            check(child, &format!("{path}.{name}"), depth + 1)?;
        }
        match map.get("required") {
            None => {}
            Some(Value::Array(names)) => {
                for name in names {
                    match name.as_str() {
                        Some(name) if properties.contains_key(name) => {}
                        _ => return fail("required must name declared properties"),
                    }
                }
            }
            Some(_) => return fail("required must be a list of names"),
        }
    }
    if kind == "array" {
        match map.get("items") {
            Some(items) => check(items, &format!("{path}[]"), depth + 1)?,
            None => return fail("an array needs items"),
        }
    }
    Ok(())
}

fn validate(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    let matches = match kind {
        "object" => value.is_object(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        _ => false,
    };
    if !matches {
        errors.push(format!("{path}: expected {kind}"));
        return;
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        errors.push(format!("{path}: not one of the allowed values"));
    }
    if let (Some(object), Some(properties)) = (
        value.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                errors.push(format!("{path}.{name}: is required"));
            }
        }
        for (name, child) in properties {
            if let Some(found) = object.get(name) {
                validate(child, found, &format!("{path}.{name}"), errors);
            }
        }
    }
    if let (Some(list), Some(items)) = (value.as_array(), schema.get("items")) {
        for (index, item) in list.iter().enumerate() {
            validate(items, item, &format!("{path}[{index}]"), errors);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema(value: Value) -> Result<JsonSchema, AiError> {
        JsonSchema::from_value(value)
    }

    fn reason(result: Result<JsonSchema, AiError>) -> String {
        match result.unwrap_err() {
            AiError::Schema(reason) => reason,
            other => panic!("not a schema error: {other}"),
        }
    }

    #[test]
    fn the_whole_subset_is_accepted() {
        let value = json!({
            "type": "object",
            "description": "a mapping",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"},
                "ratio": {"type": "number"},
                "on": {"type": "boolean"},
                "kind": {"enum": ["a", "b"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "nested": {"type": "object", "properties": {"x": {"type": "string"}}}
            },
            "required": ["name", "kind"]
        });
        assert!(schema(value).is_ok());
    }

    #[test]
    fn keywords_outside_the_subset_are_refused_with_their_path() {
        let value = json!({
            "type": "object",
            "properties": {"a": {"type": "string", "pattern": "^x"}}
        });
        let reason = reason(schema(value));
        assert!(reason.starts_with("$.a:"), "{reason}");
        assert!(reason.contains("pattern"), "{reason}");
        for keyword in ["oneOf", "$ref", "additionalProperties", "format"] {
            let value = json!({"type": "object", keyword: []});
            assert!(schema(value).is_err(), "{keyword}");
        }
    }

    #[test]
    fn the_root_must_be_an_object_schema() {
        assert!(reason(schema(json!({"type": "string"}))).contains("root"));
        assert!(schema(json!("x")).is_err());
    }

    #[test]
    fn malformed_pieces_are_refused() {
        let bad = [
            json!({"type": "object", "properties": {"a": {}}}),
            json!({"type": "object", "properties": {"a": {"type": "null"}}}),
            json!({"type": "object", "properties": {"a": {"type": "array"}}}),
            json!({"type": "object", "properties": {"a": {"enum": []}}}),
            json!({"type": "object", "properties": {"a": {"enum": [1]}}}),
            json!({"type": "object", "properties": {"a": {"type": "integer", "enum": ["x"]}}}),
            json!({"type": "object", "required": ["missing"]}),
            json!({"type": "object", "properties": {"a": {"type": "string", "items": {"type": "string"}}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "properties": {}}}}),
        ];
        for value in bad {
            assert!(schema(value.clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn nesting_beyond_the_limit_is_refused() {
        let mut node = json!({"type": "string"});
        for _ in 0..12 {
            node = json!({"type": "array", "items": node});
        }
        let value = json!({"type": "object", "properties": {"deep": node}});
        assert!(reason(schema(value)).contains("deeply"));
    }

    #[test]
    fn values_are_checked_against_the_schema_and_every_violation_is_listed() {
        let schema = schema(json!({
            "type": "object",
            "properties": {
                "kind": {"enum": ["a", "b"]},
                "count": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["kind", "count"]
        }))
        .unwrap();

        assert!(
            schema
                .validate_value(&json!({"kind": "a", "count": 2, "tags": ["x"]}))
                .is_ok()
        );
        let errors = schema
            .validate_value(&json!({"kind": "c", "tags": [1]}))
            .unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("$.kind")));
        assert!(errors.iter().any(|e| e.contains("$.count: is required")));
        assert!(errors.iter().any(|e| e.contains("$.tags[0]")));
        assert!(schema.validate_value(&json!("text")).is_err());
        assert!(
            schema
                .validate_value(&json!({"kind": "a", "count": 1.5}))
                .is_err()
        );
    }
}
