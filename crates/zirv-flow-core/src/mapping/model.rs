//! The shape of a mapping, as saved and sent over the API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::people::TraitType;

/// How the columns of a file become a person's identifiers, traits and lists.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// The column whose value is the record key, which makes a re-run of a changed file
    /// update the same records. Without it the key is the file's hash and the row number.
    #[serde(default)]
    pub key_column: Option<String>,
    pub columns: Vec<ColumnRule>,
    /// The one event each record yields, when the mapping describes one. Sync runs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventRule>,
    /// The column that marks a record as deleted in the source. A synced record whose cell
    /// satisfies the rule is removed instead of applied. Optional, and refused for CSV imports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_when: Option<DeleteWhen>,
}

impl Mapping {
    /// Whether each record also yields an event.
    pub fn has_event(&self) -> bool {
        self.event.is_some()
    }

    /// Every column or path the mapping reads, with repeats: the rule columns, the key column,
    /// the columns a `concat` joins, the columns of the event and the delete column.
    pub fn referenced_columns(&self) -> Vec<&str> {
        fn chain<'a>(columns: &mut Vec<&'a str>, transforms: &'a [Transform]) {
            for transform in transforms {
                if let Transform::Concat { columns: more, .. } = transform {
                    columns.extend(more.iter().map(String::as_str));
                }
            }
        }
        let mut columns: Vec<&str> = Vec::new();
        if let Some(key) = &self.key_column {
            columns.push(key);
        }
        for rule in &self.columns {
            columns.push(&rule.column);
            chain(&mut columns, &rule.transforms);
        }
        if let Some(event) = &self.event {
            if let Some(timestamp) = &event.timestamp {
                columns.push(&timestamp.column);
                chain(&mut columns, &timestamp.transforms);
            }
            for property in &event.properties {
                columns.push(&property.column);
                chain(&mut columns, &property.transforms);
            }
        }
        if let Some(signal) = &self.delete_when {
            columns.push(&signal.column);
        }
        columns
    }

    /// The first segment of every column or path the mapping reads, without repeats, in first-use
    /// order. A stream with no declared fields (a webhook) declares these.
    pub fn root_fields(&self) -> Vec<&str> {
        let mut roots: Vec<&str> = Vec::new();
        for column in self.referenced_columns() {
            let root = column.split('.').next().unwrap_or(column);
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }
}

/// The event a record yields: its name is fixed, the person is the record's identifiers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventRule {
    /// The event's name, a constant.
    pub name: String,
    /// Where the event happened; without it the event happened when the run saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<ValueRule>,
    /// The event's properties, each from one column.
    #[serde(default)]
    pub properties: Vec<PropertyRule>,
}

/// A column and the transforms that turn it into a value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValueRule {
    pub column: String,
    #[serde(default)]
    pub transforms: Vec<Transform>,
}

/// One property of the event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyRule {
    pub property: String,
    pub column: String,
    #[serde(default)]
    pub transforms: Vec<Transform>,
}

/// The delete signal of a mapping: a column and the rule that reads its cell.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteWhen {
    pub column: String,
    #[serde(default)]
    pub rule: DeleteRule,
}

/// How a delete column's cell is read. The set is closed: no expressions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteRule {
    /// A soft-delete flag: `true`, `1`, `yes`, `x` and so on. A cell that is not a flag is
    /// not a delete.
    #[default]
    Truthy,
    /// A soft-delete timestamp: any non-empty cell.
    Present,
}

/// What one column becomes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnRule {
    pub column: String,
    pub target: Target,
    /// Applied in order to the cell before it reaches the target.
    #[serde(default)]
    pub transforms: Vec<Transform>,
}

/// Where a column's value goes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    /// An identifier of this kind: `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    Identifier { kind: String },
    /// A trait by registry key; the registry supplies the type.
    Trait { key: String },
    /// A truthy cell (`true`, `1`, `yes`, `x`, and so on) adds the person to the list.
    List { list_id: Uuid },
    /// The column is not used.
    Ignore,
}

/// One step between a cell and its target. The set is closed: no expressions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transform {
    /// Removes the whitespace around the value.
    Trim,
    Lower,
    Upper,
    /// Converts the text to a registry type.
    Cast {
        to: TraitType,
    },
    /// Parses a date or time into a UTC timestamp. `format` is a strftime pattern such as
    /// `%d/%m/%Y %H:%M`, or `unix` or `unix_ms`. Without a time zone the value is UTC.
    Timestamp {
        format: String,
    },
    /// Joins this column's value with the other columns' values, skipping empty ones.
    Concat {
        columns: Vec<String>,
        separator: String,
    },
    /// Ignores the cell and uses this text.
    Constant {
        value: String,
    },
    /// Replaces a value that is a key of the map; any other value stays as it is.
    ValueMap {
        map: BTreeMap<String, String>,
    },
    /// Splits the text into a list of strings, trimmed and without empty parts.
    Split {
        separator: String,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn root_fields_are_the_first_segments_without_repeats() {
        let mapping: Mapping = serde_json::from_value(json!({
            "columns": [
                {"column": "user.email", "target": {"type": "identifier", "kind": "email"}},
                {"column": "user.name", "target": {"type": "trait", "key": "name"}},
                {"column": "plan", "target": {"type": "trait", "key": "plan"}}
            ],
            "event": {"name": "x", "timestamp": {"column": "at"}, "properties": [{"column": "items.0.sku", "property": "sku"}]}
        }))
        .unwrap();
        assert_eq!(mapping.root_fields(), vec!["user", "plan", "at", "items"]);
        assert!(Mapping::default().root_fields().is_empty());
    }

    #[test]
    fn a_target_is_tagged_by_type_and_a_transform_by_op() {
        let list = Uuid::nil();

        assert_eq!(
            serde_json::to_value(Target::List { list_id: list }).unwrap(),
            json!({"type": "list", "list_id": list.to_string()})
        );
        assert_eq!(
            serde_json::to_value(Transform::ValueMap {
                map: BTreeMap::from([("a".to_owned(), "b".to_owned())])
            })
            .unwrap(),
            json!({"op": "value_map", "map": {"a": "b"}})
        );
        assert_eq!(
            serde_json::from_value::<Transform>(json!({"op": "cast", "to": "string_list"}))
                .unwrap(),
            Transform::Cast {
                to: TraitType::StringList
            }
        );
    }

    #[test]
    fn a_rule_without_transforms_and_a_mapping_without_a_key_column_are_valid() {
        let mapping: Mapping = serde_json::from_value(json!({
            "columns": [{"column": "a", "target": {"type": "ignore"}}]
        }))
        .unwrap();

        assert_eq!(mapping.key_column, None);
        assert!(mapping.columns[0].transforms.is_empty());
    }

    #[test]
    fn the_delete_signal_defaults_to_truthy_and_old_mappings_still_load() {
        let old: Mapping = serde_json::from_value(json!({
            "columns": [{"column": "a", "target": {"type": "ignore"}}]
        }))
        .unwrap();
        assert_eq!(old.delete_when, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("delete_when"));

        let bare: Mapping = serde_json::from_value(json!({
            "columns": [], "delete_when": {"column": "gone"}
        }))
        .unwrap();
        assert_eq!(
            bare.delete_when,
            Some(DeleteWhen {
                column: "gone".into(),
                rule: DeleteRule::Truthy
            })
        );
        let present: DeleteWhen =
            serde_json::from_value(json!({"column": "deleted_at", "rule": "present"})).unwrap();
        assert_eq!(present.rule, DeleteRule::Present);
        assert_eq!(
            serde_json::to_value(&present).unwrap(),
            json!({"column": "deleted_at", "rule": "present"})
        );
        assert!(
            serde_json::from_value::<DeleteWhen>(json!({"column": "x", "rule": "regex"})).is_err()
        );
        assert!(serde_json::from_value::<DeleteWhen>(json!({"column": "x", "extra": 1})).is_err());
    }

    #[test]
    fn unknown_fields_unknown_types_and_unknown_operations_are_refused() {
        for bad in [
            json!({"columns": [], "extra": 1}),
            json!({"columns": [{"column": "a", "target": {"type": "ignore"}, "extra": 1}]}),
            json!({"columns": [{"column": "a", "target": {"type": "eval"}}]}),
            json!({"columns": [{"column": "a", "target": {"type": "ignore"},
                                "transforms": [{"op": "cast", "to": "integer"}]}]}),
        ] {
            assert!(
                serde_json::from_value::<Mapping>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }
}
