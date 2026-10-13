//! A source record's JSON object seen as cells, so a sync run maps a record with the same
//! rules as a CSV row.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::apply::Cells;

/// The top-level fields of a JSON object, by name.
///
/// A string is a cell as it is; a number or a boolean is a cell holding its JSON text. A null,
/// an array or an object holds no value: its cell is empty when the field is declared (the
/// stream's descriptor lists it) and missing when it is not. Nested paths come with the
/// mapping extension of the mapping editor.
#[derive(Clone, Debug, Default)]
pub struct JsonRecord {
    cells: HashMap<String, String>,
}

impl JsonRecord {
    /// Reads `object`; `declared` names the fields the stream is known to have.
    pub fn new<'a>(
        object: &Map<String, Value>,
        declared: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut cells = HashMap::with_capacity(object.len());
        for (name, value) in object {
            match value {
                Value::String(text) => {
                    cells.insert(name.clone(), text.clone());
                }
                Value::Number(_) | Value::Bool(_) => {
                    cells.insert(name.clone(), value.to_string());
                }
                Value::Null | Value::Array(_) | Value::Object(_) => {}
            }
        }
        for name in declared {
            cells.entry(name.to_owned()).or_default();
        }
        Self { cells }
    }

    /// Reads `data`, which holds no cells at all unless it is an object.
    pub fn from_value<'a>(data: &Value, declared: impl IntoIterator<Item = &'a str>) -> Self {
        match data {
            Value::Object(object) => Self::new(object, declared),
            _ => Self::new(&Map::new(), declared),
        }
    }
}

impl Cells for JsonRecord {
    fn cell(&self, column: &str) -> Option<&str> {
        self.cells.get(column).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(value: &Value, declared: &[&str]) -> JsonRecord {
        JsonRecord::from_value(value, declared.iter().copied())
    }

    #[test]
    fn strings_numbers_and_booleans_become_cells() {
        let row = record(
            &json!({"name": "Ada", "age": 36, "score": 1.5, "vip": true}),
            &[],
        );

        assert_eq!(row.cell("name"), Some("Ada"));
        assert_eq!(row.cell("age"), Some("36"));
        assert_eq!(row.cell("score"), Some("1.5"));
        assert_eq!(row.cell("vip"), Some("true"));
    }

    #[test]
    fn null_array_object_and_absent_fields_are_missing_unless_declared() {
        let value = json!({"a": null, "b": [1], "c": {"x": 1}});

        let undeclared = record(&value, &[]);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(undeclared.cell(name), None, "{name}");
        }
        let declared = record(&value, &["a", "b", "c", "d"]);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(declared.cell(name), Some(""), "{name}");
        }
    }

    #[test]
    fn a_declared_field_keeps_its_value() {
        let row = record(&json!({"email": "a@example.com"}), &["email"]);

        assert_eq!(row.cell("email"), Some("a@example.com"));
    }

    #[test]
    fn a_value_that_is_not_an_object_has_only_declared_empty_cells() {
        let row = record(&json!([1, 2]), &["email"]);

        assert_eq!(row.cell("email"), Some(""));
        assert_eq!(row.cell("other"), None);
    }
}
