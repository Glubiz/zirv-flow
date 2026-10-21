//! The field-by-field difference between a draft and the saved mapping or manifest, for the
//! reviewer (architecture 13.2). It is review presentation, so it lives here and not in core.
//!
//! Objects are compared key by key. Some arrays are lists of rules, where a position means
//! nothing: they are matched by a key field (a column's name, a stream's name) so that moving a
//! rule is not a change and a change inside a rule is reported at the field. Every other array is
//! compared whole. Paths are RFC 6901 JSON pointers with the key in place of the position.

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// The arrays of a mapping that are lists of rules, as `(path, key field)`. A `*` stands for any
/// one key.
pub(crate) const MAPPING_KEYS: &[(&str, &str)] =
    &[("/columns", "column"), ("/event/properties", "property")];

/// A segment definition has no list of rules: its arrays are conditions, where a position is the
/// condition, so every array is compared whole.
pub(crate) const SEGMENT_KEYS: &[(&str, &str)] = &[];

/// A flow draft is `{"graph", "settings"}`: the nodes and the edges are lists of rules matched by
/// their ids, so moving a node in the list is not a change and a change inside a node is reported
/// at the field. Every other array (the conditions of a branch, say) is compared whole.
pub(crate) const FLOW_KEYS: &[(&str, &str)] =
    &[("/graph/nodes", "id"), ("/graph/edges", "id")];

/// The arrays of a manifest that are lists of rules, as `(path, key field)`.
pub(crate) const MANIFEST_KEYS: &[(&str, &str)] = &[
    ("/streams", "name"),
    ("/streams/*/fields", "name"),
    ("/streams/*/mapping/columns", "column"),
];

/// What happened to a field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    Added,
    Removed,
    Changed,
}

/// One difference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub(crate) struct Change {
    /// A JSON pointer into the artifact, with the key of a rule in place of its position. A
    /// repeated key reads `key#2`.
    pub(crate) path: String,
    pub(crate) change: ChangeKind,
    /// The saved value; absent for an added field.
    #[schema(value_type = Option<Object>)]
    pub(crate) before: Option<Value>,
    /// The drafted value; absent for a removed field.
    #[schema(value_type = Option<Object>)]
    pub(crate) after: Option<Value>,
}

/// What differs between `before` (none when nothing is saved: every field is then added) and
/// `after`: object keys in sorted order, rules in their order in `after`, removed ones last.
pub(crate) fn changes(before: Option<&Value>, after: &Value, keys: &[(&str, &str)]) -> Vec<Change> {
    // A null member is an unset option, the same as an absent one.
    let before = before.map(without_nulls);
    let before = before.as_ref();
    let after = &without_nulls(after);
    let mut found = Vec::new();
    let walker = Walker { keys };
    match before {
        Some(before) => walker.walk("", Some(before), Some(after), &mut found),
        None => walker.added_fields(after, &mut found),
    }
    found
}

/// The value without its null object members, at every depth.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(key, member)| (key.clone(), without_nulls(member)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

/// Escapes a key for a JSON pointer.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

struct Walker<'a> {
    keys: &'a [(&'a str, &'a str)],
}

impl Walker<'_> {
    /// The key field of the array at `path`, when it is a list of rules.
    fn key_field(&self, path: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(pattern, _)| matches_pattern(pattern, path))
            .map(|(_, key)| *key)
    }

    fn walk(
        &self,
        path: &str,
        before: Option<&Value>,
        after: Option<&Value>,
        out: &mut Vec<Change>,
    ) {
        match (before, after) {
            (None, None) => {}
            (Some(before), None) => self.removed(path, before, out),
            (None, Some(after)) => self.added(path, after, out),
            (Some(before), Some(after)) if before == after => {}
            (Some(Value::Object(old)), Some(Value::Object(new))) => {
                for (key, value) in new {
                    self.walk(
                        &format!("{path}/{}", escape(key)),
                        old.get(key),
                        Some(value),
                        out,
                    );
                }
                for (key, value) in old {
                    if !new.contains_key(key) {
                        self.removed(&format!("{path}/{}", escape(key)), value, out);
                    }
                }
            }
            (Some(whole_old @ Value::Array(old)), Some(whole_new @ Value::Array(new))) => {
                match self
                    .key_field(path)
                    .and_then(|field| Some((keyed(old, field)?, keyed(new, field)?)))
                {
                    Some((old, new)) => {
                        for (key, value) in &new {
                            let found = old.iter().find(|(old_key, _)| old_key == key);
                            self.walk(
                                &format!("{path}/{}", escape(key)),
                                found.map(|(_, value)| *value),
                                Some(value),
                                out,
                            );
                        }
                        for (key, value) in &old {
                            if !new.iter().any(|(new_key, _)| new_key == key) {
                                self.removed(&format!("{path}/{}", escape(key)), value, out);
                            }
                        }
                    }
                    None => out.push(changed(path, whole_old, whole_new)),
                }
            }
            (Some(before), Some(after)) => out.push(changed(path, before, after)),
        }
    }

    /// A field that was not saved: a list of rules is listed rule by rule, anything else whole.
    fn added(&self, path: &str, after: &Value, out: &mut Vec<Change>) {
        if let (Some(field), Value::Array(items)) = (self.key_field(path), after)
            && let Some(rules) = keyed(items, field)
        {
            for (key, value) in rules {
                out.push(Change {
                    path: format!("{path}/{}", escape(&key)),
                    change: ChangeKind::Added,
                    before: None,
                    after: Some(value.clone()),
                });
            }
            return;
        }
        out.push(Change {
            path: path.to_owned(),
            change: ChangeKind::Added,
            before: None,
            after: Some(after.clone()),
        });
    }

    fn removed(&self, path: &str, before: &Value, out: &mut Vec<Change>) {
        out.push(Change {
            path: path.to_owned(),
            change: ChangeKind::Removed,
            before: Some(before.clone()),
            after: None,
        });
    }

    /// Everything in an artifact with nothing saved: each top-level field is added.
    fn added_fields(&self, after: &Value, out: &mut Vec<Change>) {
        match after {
            Value::Object(fields) => {
                for (key, value) in fields {
                    self.added(&format!("/{}", escape(key)), value, out);
                }
            }
            other => self.added("", other, out),
        }
    }
}

fn changed(path: &str, before: &Value, after: &Value) -> Change {
    Change {
        path: path.to_owned(),
        change: ChangeKind::Changed,
        before: Some(before.clone()),
        after: Some(after.clone()),
    }
}

/// The elements of a list of rules under their keys, with a repeated key numbered `key#2`. None
/// when an element has no text key, so that the array is compared whole instead.
fn keyed<'a>(items: &'a [Value], field: &str) -> Option<Vec<(String, &'a Value)>> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let key = item.get(field)?.as_str()?.to_owned();
        let count = match seen.iter_mut().find(|(seen_key, _)| *seen_key == key) {
            Some((_, count)) => {
                *count += 1;
                *count
            }
            None => {
                seen.push((key.clone(), 1));
                1
            }
        };
        let name = if count == 1 {
            key
        } else {
            format!("{key}#{count}")
        };
        out.push((name, item));
    }
    Some(out)
}

/// Whether `path` is the array at `pattern`, where `*` stands for any one segment.
fn matches_pattern(pattern: &str, path: &str) -> bool {
    let (mut pattern, mut path) = (pattern.split('/'), path.split('/'));
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            (Some("*"), Some(_)) => {}
            (Some(expected), Some(found)) if expected == found => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn paths(found: &[Change]) -> Vec<(&str, ChangeKind)> {
        found
            .iter()
            .map(|change| (change.path.as_str(), change.change))
            .collect()
    }

    #[test]
    fn rules_match_by_column_and_report_leaf_paths() {
        let saved = json!({"columns": [
            {"column": "Email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "Plan", "target": {"type": "trait", "key": "plan"}},
            {"column": "Old", "target": {"type": "ignore"}}
        ]});
        // Reordered, one target changed, one rule dropped, one added.
        let draft = json!({"columns": [
            {"column": "Plan", "target": {"type": "trait", "key": "tier"}},
            {"column": "Email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "Name", "target": {"type": "trait", "key": "name"}}
        ]});

        let found = changes(Some(&saved), &draft, MAPPING_KEYS);

        assert_eq!(
            paths(&found),
            [
                ("/columns/Plan/target/key", ChangeKind::Changed),
                ("/columns/Name", ChangeKind::Added),
                ("/columns/Old", ChangeKind::Removed),
            ]
        );
        assert_eq!(found[0].before, Some(json!("plan")));
        assert_eq!(found[0].after, Some(json!("tier")));
        assert_eq!(found[1].before, None);
        assert_eq!(found[2].after, None);
        assert!(changes(Some(&saved), &saved, MAPPING_KEYS).is_empty());
    }

    #[test]
    fn a_repeated_key_is_numbered_and_event_properties_match_by_property() {
        let saved = json!({"columns": [
            {"column": "A", "target": {"type": "ignore"}},
            {"column": "A", "target": {"type": "ignore"}}
        ], "event": {"name": "seen", "properties": [{"property": "sku", "column": "s"}]}});
        let draft = json!({"columns": [
            {"column": "A", "target": {"type": "ignore"}},
            {"column": "A", "target": {"type": "trait", "key": "x"}}
        ], "event": {"name": "seen", "properties": [{"property": "sku", "column": "t"}]}});

        let found = changes(Some(&saved), &draft, MAPPING_KEYS);

        assert_eq!(
            paths(&found),
            [
                ("/columns/A#2/target/key", ChangeKind::Added),
                ("/columns/A#2/target/type", ChangeKind::Changed),
                ("/event/properties/sku/column", ChangeKind::Changed),
            ]
        );
    }

    #[test]
    fn streams_match_by_name() {
        let saved = json!({"base_url": "https://a.test", "streams": [
            {"name": "contacts", "fields": [{"name": "id", "type": "string"}]},
            {"name": "orders", "fields": []}
        ]});
        let draft = json!({"base_url": "https://a.test", "streams": [
            {"name": "orders", "fields": []},
            {"name": "contacts", "fields": [
                {"name": "id", "type": "number"}, {"name": "n", "type": "string"}]}
        ]});

        let found = changes(Some(&saved), &draft, MANIFEST_KEYS);

        assert_eq!(
            paths(&found),
            [
                ("/streams/contacts/fields/id/type", ChangeKind::Changed),
                ("/streams/contacts/fields/n", ChangeKind::Added),
            ]
        );
    }

    #[test]
    fn pointer_keys_are_escaped() {
        let saved = json!({"columns": [{"column": "a/b~c", "target": {"type": "ignore"}}]});
        let draft = json!({"columns": [{"column": "a/b~c", "target": {"type": "trait"}}]});

        let found = changes(Some(&saved), &draft, MAPPING_KEYS);

        assert_eq!(found[0].path, "/columns/a~1b~0c/target/type");
        let objects = changes(Some(&json!({"a/b": 1})), &json!({"a/b": 2}), MAPPING_KEYS);
        assert_eq!(objects[0].path, "/a~1b");
    }

    #[test]
    fn a_null_member_is_the_same_as_an_absent_one() {
        let saved = json!({"columns": [], "key_column": null});
        let draft = json!({"columns": []});
        assert!(changes(Some(&saved), &draft, MAPPING_KEYS).is_empty());
        assert!(changes(None, &json!({"key_column": null}), MAPPING_KEYS).is_empty());
    }

    #[test]
    fn no_base_means_all_added() {
        let draft = json!({"key_column": "id", "columns": [
            {"column": "Email", "target": {"type": "ignore"}},
            {"column": "Plan", "target": {"type": "ignore"}}
        ], "event": {"name": "seen"}});

        let found = changes(None, &draft, MAPPING_KEYS);

        assert_eq!(
            paths(&found),
            [
                ("/columns/Email", ChangeKind::Added),
                ("/columns/Plan", ChangeKind::Added),
                ("/event", ChangeKind::Added),
                ("/key_column", ChangeKind::Added),
            ]
        );
        assert!(found.iter().all(|change| change.before.is_none()));
    }

    #[test]
    fn other_arrays_compare_whole_and_a_keyless_list_falls_back_to_whole() {
        let saved = json!({"primary_key": ["a"], "columns": [{"target": 1}]});
        let draft = json!({"primary_key": ["a", "b"], "columns": [{"target": 2}]});

        let found = changes(Some(&saved), &draft, MAPPING_KEYS);

        assert_eq!(
            paths(&found),
            [
                ("/columns", ChangeKind::Changed),
                ("/primary_key", ChangeKind::Changed)
            ]
        );
    }
}
