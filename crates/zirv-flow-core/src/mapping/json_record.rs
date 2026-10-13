//! A source record's JSON object seen as cells, so a sync run maps a record with the same
//! rules as a CSV row.

use hashbrown::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::apply::Cells;
use super::model::Mapping;

/// The fields of a JSON object, by name or by dotted path.
///
/// A string is a cell as it is; a number or a boolean is a cell holding its JSON text. A null,
/// an array or an object holds no value: its cell is empty when the field is declared (the
/// stream's descriptor lists it) and missing when it is not.
///
/// A column that is a dotted path (`address.city`, `items.0.sku`) is read by
/// [`JsonRecord::for_mapping`]: a top-level field with exactly that name wins; otherwise the
/// path is walked, a segment of digits indexing an array and any other segment selecting an
/// object key. A path whose first segment is declared and that leads nowhere is an empty cell;
/// one whose first segment is not declared is missing.
#[derive(Clone, Debug, Default)]
pub struct JsonRecord {
    cells: HashMap<String, String>,
}

/// The text a leaf value is a cell of: a string as it is, a number or a boolean as its JSON
/// text, anything else nothing.
fn leaf(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Walks `path` from `root` as mapping paths are walked; a webhook's dedupe field is resolved
/// with it.
pub fn walk_path<'a>(root: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    walk(root, path)
}

/// Walks `path` from `root`: a segment of digits indexes an array, any other selects a key.
fn walk<'a>(root: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let mut current = root.get(segments.next()?)?;
    for segment in segments {
        current = match current {
            Value::Object(object) => object.get(segment)?,
            Value::Array(items) => {
                if segment.is_empty() || !segment.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                items.get(segment.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

impl JsonRecord {
    /// Reads `object` for `mapping`: the top-level fields as [`JsonRecord::new`] does, and
    /// every dotted path the mapping names, resolved once. `declared` names the fields the
    /// stream is known to have.
    pub fn for_mapping<'a>(
        object: &Map<String, Value>,
        mapping: &Mapping,
        declared: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let declared: HashSet<&str> = declared.into_iter().collect();
        let mut record = Self::new(object, declared.iter().copied());
        for column in mapping.referenced_columns() {
            if object.contains_key(column) || record.cells.contains_key(column) {
                continue;
            }
            let first = column.split('.').next().unwrap_or(column);
            match walk(object, column).and_then(leaf) {
                Some(text) => {
                    record.cells.insert(column.to_owned(), text);
                }
                None if declared.contains(first) => {
                    record.cells.insert(column.to_owned(), String::new());
                }
                None => {}
            }
        }
        record
    }

    /// As [`JsonRecord::for_mapping`] over a value, which holds no cells at all unless it is an
    /// object.
    pub fn from_value_for_mapping<'a>(
        data: &Value,
        mapping: &Mapping,
        declared: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        match data {
            Value::Object(object) => Self::for_mapping(object, mapping, declared),
            _ => Self::for_mapping(&Map::new(), mapping, declared),
        }
    }

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

    use super::super::model::{ColumnRule, EventRule, PropertyRule, Target, Transform, ValueRule};
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

    fn mapping_over(columns: &[&str]) -> Mapping {
        Mapping {
            columns: columns
                .iter()
                .map(|column| ColumnRule {
                    column: (*column).to_owned(),
                    target: Target::Ignore,
                    transforms: Vec::new(),
                })
                .collect(),
            ..Mapping::default()
        }
    }

    fn pathed(value: &Value, columns: &[&str], declared: &[&str]) -> JsonRecord {
        JsonRecord::from_value_for_mapping(value, &mapping_over(columns), declared.iter().copied())
    }

    #[test]
    fn a_dotted_path_walks_objects_and_indexes_arrays() {
        let value = json!({
            "profile": {"email": "a@example.com", "age": 36, "vip": false},
            "orders": [{"total": 12.5}, {"total": 3}]
        });

        let row = pathed(
            &value,
            &[
                "profile.email",
                "profile.age",
                "profile.vip",
                "orders.0.total",
                "orders.1.total",
            ],
            &["profile", "orders"],
        );

        assert_eq!(row.cell("profile.email"), Some("a@example.com"));
        assert_eq!(row.cell("profile.age"), Some("36"));
        assert_eq!(row.cell("profile.vip"), Some("false"));
        assert_eq!(row.cell("orders.0.total"), Some("12.5"));
        assert_eq!(row.cell("orders.1.total"), Some("3"));
    }

    #[test]
    fn a_top_level_field_named_with_a_dot_wins_over_the_path() {
        let value = json!({"a.b": "literal", "a": {"b": "walked"}});

        let row = pathed(&value, &["a.b"], &["a.b", "a"]);

        assert_eq!(row.cell("a.b"), Some("literal"));
    }

    #[test]
    fn a_path_that_leads_nowhere_is_empty_when_its_first_segment_is_declared() {
        let value = json!({
            "a": {"n": null, "list": [1], "scalar": "x"},
            "s": "text",
            "arr": [1, 2]
        });
        let columns = [
            "a.missing",
            "a.n",
            "a.list.0.x",
            "a.list.key",
            "a.list.5",
            "a.scalar.deeper",
            "s.x",
            "arr.name",
            "arr.-1",
            "a.list",
            "a",
            "gone.x",
        ];

        let row = pathed(&value, &columns, &["a", "s", "arr", "gone"]);

        for column in columns {
            assert_eq!(row.cell(column), Some(""), "{column}");
        }
    }

    #[test]
    fn a_path_whose_first_segment_is_not_declared_and_leads_nowhere_is_missing() {
        let value = json!({"a": {"n": null}});

        let row = pathed(&value, &["a.missing", "a.n", "zzz.x", "zzz"], &[]);

        for column in ["a.missing", "a.n", "zzz.x", "zzz"] {
            assert_eq!(row.cell(column), None, "{column}");
        }
    }

    #[test]
    fn a_path_found_in_an_undeclared_field_has_its_value() {
        let row = pathed(&json!({"a": {"b": "v"}}), &["a.b"], &[]);

        assert_eq!(row.cell("a.b"), Some("v"));
    }

    #[test]
    fn the_columns_of_concat_key_and_event_are_resolved_too() {
        let mut mapping = mapping_over(&["first"]);
        mapping.columns[0].transforms = vec![Transform::Concat {
            columns: vec!["name.last".into()],
            separator: " ".into(),
        }];
        mapping.key_column = Some("ids.0".into());
        mapping.event = Some(EventRule {
            name: "order".into(),
            timestamp: Some(ValueRule {
                column: "at.when".into(),
                transforms: Vec::new(),
            }),
            properties: vec![PropertyRule {
                property: "total".into(),
                column: "cart.total".into(),
                transforms: vec![Transform::Concat {
                    columns: vec!["cart.currency".into()],
                    separator: " ".into(),
                }],
            }],
        });
        let value = json!({
            "first": "Ada",
            "name": {"last": "Lovelace"},
            "ids": ["k1"],
            "at": {"when": "2026-01-01T00:00:00Z"},
            "cart": {"total": 9, "currency": "EUR"}
        });

        let row = JsonRecord::from_value_for_mapping(
            &value,
            &mapping,
            ["first", "name", "ids", "at", "cart"],
        );

        for (column, expected) in [
            ("name.last", "Lovelace"),
            ("ids.0", "k1"),
            ("at.when", "2026-01-01T00:00:00Z"),
            ("cart.total", "9"),
            ("cart.currency", "EUR"),
        ] {
            assert_eq!(row.cell(column), Some(expected), "{column}");
        }
    }

    #[test]
    fn new_still_reads_only_top_level_fields() {
        let row = record(&json!({"a": {"b": "v"}}), &[]);

        assert_eq!(row.cell("a.b"), None);
        assert_eq!(row.cell("a"), None);
    }
}
