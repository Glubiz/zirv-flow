//! Ingest redaction (architecture section 12.1): drop or keyed-hash configured properties
//! and truncate IP addresses before an event or a reject payload is stored.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{HmacKey, ValidationError, WorkspaceId};

const MAX_PATHS: usize = 50;
const MAX_PATH_SEGMENTS: usize = 8;
const MAX_SEGMENT_CHARS: usize = 128;

/// Which properties are redacted. Paths are dot-separated and reach into nested objects.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RedactionRules {
    /// Removed.
    pub drop: Vec<String>,
    /// Replaced by a keyed hash.
    pub hash: Vec<String>,
    /// An IP address with its host part zeroed; a value that is not an address is removed.
    pub truncate_ip: Vec<String>,
}

impl Default for RedactionRules {
    fn default() -> Self {
        Self {
            drop: Vec::new(),
            hash: Vec::new(),
            truncate_ip: vec!["ip".to_owned(), "context.ip".to_owned()],
        }
    }
}

/// Checks the paths of a rule set.
pub fn validate_rules(rules: &RedactionRules) -> Result<(), ValidationError> {
    for paths in [&rules.drop, &rules.hash, &rules.truncate_ip] {
        let valid = paths.len() <= MAX_PATHS
            && paths.iter().all(|path| {
                let segments: Vec<&str> = path.split('.').collect();
                segments.len() <= MAX_PATH_SEGMENTS
                    && segments.iter().all(|segment| {
                        !segment.is_empty()
                            && segment.chars().count() <= MAX_SEGMENT_CHARS
                            && !segment.chars().any(char::is_control)
                    })
            });
        if !valid {
            return Err(ValidationError::new(
                "redaction",
                "each list must hold at most 50 dot-separated paths of at most 8 non-empty segments",
            ));
        }
    }
    Ok(())
}

/// Applies the rules in place: drop, then hash, then truncate. A path that is absent is
/// skipped. A hash is the lowercase hex of the HMAC of the workspace id and the canonical
/// JSON of the value.
pub fn redact(
    properties: &mut Map<String, Value>,
    rules: &RedactionRules,
    workspace: WorkspaceId,
    key: &HmacKey,
) {
    for path in &rules.drop {
        edit(properties, path, &mut |_| None);
    }
    for path in &rules.hash {
        edit(properties, path, &mut |value| {
            let mut message = workspace.as_uuid().as_bytes().to_vec();
            canonical_json(&value, &mut message);
            let digest = key.digest(&message);
            Some(Value::String(
                digest.iter().map(|byte| format!("{byte:02x}")).collect(),
            ))
        });
    }
    for path in &rules.truncate_ip {
        edit(properties, path, &mut |value| {
            value.as_str().and_then(truncate_ip).map(Value::String)
        });
    }
}

/// Replaces (`Some`) or removes (`None`) the value at `path`, if there is one.
fn edit(
    properties: &mut Map<String, Value>,
    path: &str,
    change: &mut dyn FnMut(Value) -> Option<Value>,
) {
    // A key with dots in it is a path of one segment, and the same text is also a nested path:
    // both copies are edited.
    if path.contains('.')
        && let Some(value) = properties.remove(path)
        && let Some(replacement) = change(value)
    {
        properties.insert(path.to_owned(), replacement);
    }
    let segments: Vec<&str> = path.split('.').collect();
    edit_in(properties, &segments, change);
}

fn edit_in(
    object: &mut Map<String, Value>,
    segments: &[&str],
    change: &mut dyn FnMut(Value) -> Option<Value>,
) {
    match segments {
        [] => {}
        [last] => {
            if let Some(value) = object.remove(*last)
                && let Some(replacement) = change(value)
            {
                object.insert((*last).to_owned(), replacement);
            }
        }
        [first, rest @ ..] => match object.get_mut(*first) {
            Some(Value::Object(inner)) => edit_in(inner, rest, change),
            Some(Value::Array(items)) => edit_items(items, rest, change),
            _ => {}
        },
    }
}

/// Applies the rest of a path to every object in a list, however deeply lists are nested.
fn edit_items(
    items: &mut [Value],
    segments: &[&str],
    change: &mut dyn FnMut(Value) -> Option<Value>,
) {
    for item in items {
        match item {
            Value::Object(inner) => edit_in(inner, segments, change),
            Value::Array(nested) => edit_items(nested, segments, change),
            _ => {}
        }
    }
}

fn truncate_ip(text: &str) -> Option<String> {
    match text.parse::<IpAddr>().ok()? {
        IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            Some(Ipv4Addr::new(a, b, c, 0).to_string())
        }
        IpAddr::V6(address) => {
            let [a, b, c, ..] = address.segments();
            Some(Ipv6Addr::new(a, b, c, 0, 0, 0, 0, 0).to_string())
        }
    }
}

/// Compact JSON with object keys in sorted order, so equal values hash equally.
fn canonical_json(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(Value::String(key.clone()).to_string().as_bytes());
                out.push(b':');
                canonical_json(&map[key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical_json(item, out);
            }
            out.push(b']');
        }
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{HmacPurpose, MasterKey};

    fn key() -> HmacKey {
        MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=")
            .unwrap()
            .hmac_key(HmacPurpose::Redaction)
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn rules(drop: &[&str], hash: &[&str], truncate_ip: &[&str]) -> RedactionRules {
        let owned = |paths: &[&str]| paths.iter().map(|path| (*path).to_owned()).collect();
        RedactionRules {
            drop: owned(drop),
            hash: owned(hash),
            truncate_ip: owned(truncate_ip),
        }
    }

    #[test]
    fn drop_removes_top_level_and_nested_values_and_skips_absent_paths() {
        let mut properties = map(json!({"card": "4111", "ctx": {"ssn": "1", "ok": 2}}));

        redact(
            &mut properties,
            &rules(&["card", "ctx.ssn", "nope.deeper"], &[], &[]),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(Value::Object(properties), json!({"ctx": {"ok": 2}}));
    }

    #[test]
    fn a_path_also_matches_a_flat_key_with_dots_in_it() {
        let mut properties =
            map(json!({"context.ip": "203.0.113.9", "context.card": "4111", "a": {"b": 1}}));

        redact(
            &mut properties,
            &rules(&["context.card"], &[], &["context.ip"]),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(
            Value::Object(properties),
            json!({"context.ip": "203.0.113.0", "a": {"b": 1}})
        );
    }

    #[test]
    fn a_flat_key_and_a_nested_copy_at_the_same_path_are_both_redacted() {
        let mut properties = map(json!({
            "context.ip": "203.0.113.9",
            "context": {"ip": "198.51.100.7"}
        }));

        redact(
            &mut properties,
            &rules(&[], &[], &["context.ip"]),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(
            Value::Object(properties),
            json!({"context.ip": "203.0.113.0", "context": {"ip": "198.51.100.0"}})
        );
    }

    #[test]
    fn a_path_walks_arrays_of_arrays() {
        let mut properties = map(json!({
            "rows": [[{"ip": "203.0.113.9", "keep": 1}], [[{"ip": "198.51.100.7"}]], "loose"]
        }));

        redact(
            &mut properties,
            &rules(&[], &[], &["rows.ip"]),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(
            Value::Object(properties),
            json!({"rows": [[{"ip": "203.0.113.0", "keep": 1}], [[{"ip": "198.51.100.0"}]], "loose"]})
        );
    }

    #[test]
    fn a_path_applies_to_every_object_in_an_array() {
        let mut properties = map(json!({
            "items": [{"sku": "a", "card": "1"}, "loose", {"sku": "b"}],
            "ctx": {"users": [{"ip": "198.51.100.7"}, {"ip": "nope"}]}
        }));

        redact(
            &mut properties,
            &rules(&["items.card"], &["items.sku"], &["ctx.users.ip"]),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(properties["items"][0].get("card"), None);
        assert_eq!(properties["items"][0]["sku"].as_str().unwrap().len(), 64);
        assert_eq!(properties["items"][1], "loose");
        assert_eq!(properties["items"][2]["sku"].as_str().unwrap().len(), 64);
        assert_eq!(
            properties["ctx"]["users"],
            json!([{"ip": "198.51.100.0"}, {}])
        );
    }

    #[test]
    fn hash_replaces_a_value_with_a_stable_workspace_bound_digest() {
        let one = WorkspaceId::new();
        let two = WorkspaceId::new();
        let rule = rules(&[], &["email"], &[]);
        let hashed = |workspace, value: Value| {
            let mut properties = map(json!({ "email": value }));
            redact(&mut properties, &rule, workspace, &key());
            properties["email"].as_str().unwrap().to_owned()
        };

        let first = hashed(one, json!("a@example.com"));

        assert_eq!(first.len(), 64);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(first, hashed(one, json!("a@example.com")));
        assert_ne!(first, hashed(one, json!("b@example.com")));
        assert_ne!(first, hashed(two, json!("a@example.com")));
        assert!(!first.contains("example"));
    }

    #[test]
    fn hashing_ignores_the_key_order_of_an_object_value() {
        let rule = rules(&[], &["v"], &[]);
        let workspace = WorkspaceId::new();
        let mut a = map(json!({"v": {"x": 1, "y": [1, 2]}}));
        let mut b = map(json!({"v": {"y": [1, 2], "x": 1}}));

        redact(&mut a, &rule, workspace, &key());
        redact(&mut b, &rule, workspace, &key());

        assert_eq!(a, b);
    }

    #[test]
    fn ip_addresses_lose_their_host_part() {
        let mut properties = map(json!({
            "ip": "203.0.113.77",
            "context": {"ip": "2001:db8:abcd:1:2:3:4:5"},
            "other": "198.51.100.9"
        }));

        redact(
            &mut properties,
            &RedactionRules::default(),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(properties["ip"], "203.0.113.0");
        assert_eq!(properties["context"]["ip"], "2001:db8:abcd::");
        assert_eq!(properties["other"], "198.51.100.9");
    }

    #[test]
    fn a_value_that_is_not_an_ip_is_dropped() {
        let mut properties = map(json!({"ip": "not an ip", "context": {"ip": 5}}));

        redact(
            &mut properties,
            &RedactionRules::default(),
            WorkspaceId::new(),
            &key(),
        );

        assert_eq!(Value::Object(properties), json!({"context": {}}));
    }

    #[test]
    fn the_default_rules_truncate_ips_only_and_missing_keys_use_them() {
        let parsed: RedactionRules = serde_json::from_value(json!({"drop": ["a"]})).unwrap();

        assert_eq!(parsed.drop, ["a"]);
        assert_eq!(parsed.truncate_ip, ["ip", "context.ip"]);
        assert!(RedactionRules::default().drop.is_empty());
    }

    #[test]
    fn paths_are_checked() {
        assert!(validate_rules(&RedactionRules::default()).is_ok());
        for bad in ["", "a..b", ".a", "a.", &"a.".repeat(9)] {
            let rules = rules(&[bad], &[], &[]);
            assert!(validate_rules(&rules).is_err(), "{bad}");
        }
        let many = RedactionRules {
            hash: vec!["a".to_owned(); 51],
            ..RedactionRules::default()
        };
        assert!(validate_rules(&many).is_err());
    }
}
