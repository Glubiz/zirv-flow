//! Event-forwarding rules (architecture 12.5): the typed mapping from an accepted event to a
//! destination's payload, the filter vocabulary, and the idempotency key. Pure: the store
//! evaluates both in SQL, inside the transaction that writes the event.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::segment::{Node, Op, PropertyFilter, parse};

/// Active rules a workspace may hold.
pub const MAX_ACTIVE_RULES: i64 = 20;
/// Output keys in one mapping.
pub const MAX_MAPPING_KEYS: usize = 50;
/// Characters in an output key.
pub const MAX_KEY_CHARS: usize = 64;
/// Characters in a JSON pointer.
pub const MAX_POINTER_CHARS: usize = 256;
/// Reference tokens in a JSON pointer.
pub const MAX_POINTER_TOKENS: usize = 8;
/// Characters in a static string.
pub const MAX_STATIC_CHARS: usize = 1024;
/// Property predicates in one filter.
pub const MAX_FILTERS: usize = 10;

/// Issue codes of a mapping or a filter; each is a static string.
pub mod code {
    pub const NOT_AN_OBJECT: &str = "not_an_object";
    pub const EMPTY: &str = "empty";
    pub const TOO_MANY: &str = "too_many";
    pub const INVALID_KEY: &str = "invalid_key";
    pub const INVALID_SOURCE: &str = "invalid_source";
    pub const UNKNOWN_SOURCE: &str = "unknown_source";
    pub const INVALID_POINTER: &str = "invalid_pointer";
    pub const INVALID_VALUE: &str = "invalid_value";
    pub const OP_NOT_SUPPORTED: &str = "op_not_supported";
    pub const INVALID_FILTER: &str = "invalid_filter";
}

/// A problem in a mapping or a filter: where, and a fixed code. Never a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleIssue {
    pub pointer: String,
    pub code: &'static str,
}

impl RuleIssue {
    fn new(pointer: impl Into<String>, code: &'static str) -> Self {
        Self {
            pointer: pointer.into(),
            code,
        }
    }
}

/// A scalar field of the event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventField {
    Id,
    Name,
    Time,
    ReceivedAt,
}

impl EventField {
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Id => "event.id",
            Self::Name => "event.name",
            Self::Time => "event.time",
            Self::ReceivedAt => "event.received_at",
        }
    }
}

/// Where one output value comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Event(EventField),
    PersonId,
    /// `event.properties` at a JSON pointer, as its tokens. Absent reads as null.
    Property(Vec<String>),
    /// A string, number or boolean.
    Static(Value),
}

/// The mapping: output key to source, in key order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mapping {
    entries: BTreeMap<String, Source>,
}

impl Mapping {
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Source)> {
        self.entries
            .iter()
            .map(|(key, source)| (key.as_str(), source))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parses the wire form: an object of output key to `{"from": "event.id"}`,
    /// `{"from": "event.properties", "pointer": "/a/b"}` or `{"value": <string|number|bool>}`.
    pub fn parse(value: &Value) -> Result<Self, Vec<RuleIssue>> {
        let Some(object) = value.as_object() else {
            return Err(vec![RuleIssue::new("", code::NOT_AN_OBJECT)]);
        };
        if object.is_empty() {
            return Err(vec![RuleIssue::new("", code::EMPTY)]);
        }
        if object.len() > MAX_MAPPING_KEYS {
            return Err(vec![RuleIssue::new("", code::TOO_MANY)]);
        }
        let mut issues = Vec::new();
        let mut entries = BTreeMap::new();
        for (key, spec) in object {
            let at = format!("/{}", escape_token(key));
            if !valid_key(key) {
                issues.push(RuleIssue::new(at.clone(), code::INVALID_KEY));
            }
            match source_of(spec, &at) {
                Ok(source) => {
                    entries.insert(key.clone(), source);
                }
                Err(issue) => issues.push(issue),
            }
        }
        if issues.is_empty() {
            Ok(Self { entries })
        } else {
            Err(issues)
        }
    }

    /// The wire form, which [`Mapping::parse`] reads back.
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        for (key, source) in &self.entries {
            let spec = match source {
                Source::Event(field) => json!({"from": field.wire()}),
                Source::PersonId => json!({"from": "person.id"}),
                Source::Property(tokens) => {
                    let pointer: String = tokens
                        .iter()
                        .map(|token| format!("/{}", escape_token(token)))
                        .collect();
                    json!({"from": "event.properties", "pointer": pointer})
                }
                Source::Static(value) => json!({"value": value}),
            };
            object.insert(key.clone(), spec);
        }
        Value::Object(object)
    }
}

fn valid_key(key: &str) -> bool {
    let length = key.chars().count();
    (1..=MAX_KEY_CHARS).contains(&length)
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn source_of(spec: &Value, at: &str) -> Result<Source, RuleIssue> {
    let bad = |code| RuleIssue::new(at, code);
    let Some(object) = spec.as_object() else {
        return Err(bad(code::INVALID_SOURCE));
    };
    if let Some(value) = object.get("value") {
        if object.len() != 1 {
            return Err(bad(code::INVALID_SOURCE));
        }
        let fine = match value {
            Value::String(text) => text.chars().count() <= MAX_STATIC_CHARS && !text.contains('\0'),
            Value::Number(_) | Value::Bool(_) => true,
            _ => false,
        };
        return if fine {
            Ok(Source::Static(value.clone()))
        } else {
            Err(bad(code::INVALID_VALUE))
        };
    }
    let Some(from) = object.get("from").and_then(Value::as_str) else {
        return Err(bad(code::INVALID_SOURCE));
    };
    let plain = |source| {
        if object.len() == 1 {
            Ok(source)
        } else {
            Err(bad(code::INVALID_SOURCE))
        }
    };
    match from {
        "event.id" => plain(Source::Event(EventField::Id)),
        "event.name" => plain(Source::Event(EventField::Name)),
        "event.time" => plain(Source::Event(EventField::Time)),
        "event.received_at" => plain(Source::Event(EventField::ReceivedAt)),
        "person.id" => plain(Source::PersonId),
        "event.properties" => {
            if object.len() != 2 {
                return Err(bad(code::INVALID_SOURCE));
            }
            let pointer = object.get("pointer").and_then(Value::as_str);
            pointer
                .and_then(pointer_tokens)
                .map(Source::Property)
                .ok_or_else(|| RuleIssue::new(format!("{at}/pointer"), code::INVALID_POINTER))
        }
        _ => Err(bad(code::UNKNOWN_SOURCE)),
    }
}

/// The tokens of an RFC 6901 pointer that names at least one property; `None` when it is
/// malformed or exceeds the limits.
fn pointer_tokens(pointer: &str) -> Option<Vec<String>> {
    if pointer.chars().count() > MAX_POINTER_CHARS || pointer.contains('\0') {
        return None;
    }
    let rest = pointer.strip_prefix('/')?;
    let mut tokens = Vec::new();
    for raw in rest.split('/') {
        let mut token = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '~' {
                match chars.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    _ => return None,
                }
            } else {
                token.push(c);
            }
        }
        if token.is_empty() {
            return None;
        }
        tokens.push(token);
    }
    (tokens.len() <= MAX_POINTER_TOKENS).then_some(tokens)
}

/// The operators a forwarding filter supports.
pub const FILTER_OPS: [Op; 13] = [
    Op::Eq,
    Op::Neq,
    Op::In,
    Op::NotIn,
    Op::Contains,
    Op::StartsWith,
    Op::Exists,
    Op::NotExists,
    Op::Gt,
    Op::Gte,
    Op::Lt,
    Op::Lte,
    Op::Between,
];

/// The segment definition that holds `event_name` with the filter, for the segment validator.
pub fn filter_definition(event_name: &str, filter: &Value) -> Value {
    json!({"v": 1, "event": event_name, "where": filter})
}

/// Parses a filter (the segment `where` list) and checks that its operators are supported.
/// Whether the properties exist and the values fit their types is the segment validator's check
/// ([`filter_definition`]).
pub fn parse_filter(
    event_name: &str,
    filter: &Value,
) -> Result<Vec<PropertyFilter>, Vec<RuleIssue>> {
    let Some(list) = filter.as_array() else {
        return Err(vec![RuleIssue::new("", code::NOT_AN_OBJECT)]);
    };
    if list.len() > MAX_FILTERS {
        return Err(vec![RuleIssue::new("", code::TOO_MANY)]);
    }
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let definition = parse(&filter_definition(event_name, filter)).map_err(|issues| {
        issues
            .into_iter()
            .map(|issue| RuleIssue::new(issue.pointer, code::INVALID_FILTER))
            .collect::<Vec<_>>()
    })?;
    let Node::Event(condition) = definition.root else {
        return Err(vec![RuleIssue::new("", code::INVALID_FILTER)]);
    };
    if condition.agg.is_some() || condition.window.is_some() {
        return Err(vec![RuleIssue::new("", code::INVALID_FILTER)]);
    }
    let issues: Vec<RuleIssue> = condition
        .filters
        .iter()
        .enumerate()
        .filter(|(_, filter)| !FILTER_OPS.contains(&filter.op))
        .map(|(index, _)| RuleIssue::new(format!("/{index}/op"), code::OP_NOT_SUPPORTED))
        .collect();
    if issues.is_empty() {
        Ok(condition.filters)
    } else {
        Err(issues)
    }
}

/// The idempotency key of one event delivered to one destination: stable across retries and
/// re-drives. The outbox insert computes the same value in SQL.
pub fn idempotency_key(event: Uuid, destination: Uuid) -> String {
    let digest = Sha256::digest(format!("{event}:{destination}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_reads_every_source_kind_and_round_trips() {
        let wire = json!({
            "id": {"from": "event.id"},
            "name": {"from": "event.name"},
            "at": {"from": "event.time"},
            "seen": {"from": "event.received_at"},
            "who": {"from": "person.id"},
            "plan": {"from": "event.properties", "pointer": "/plan/tier"},
            "src": {"value": "shop"},
            "n": {"value": 3},
            "flag": {"value": true},
        });

        let mapping = Mapping::parse(&wire).unwrap();

        assert_eq!(mapping.len(), 9);
        assert_eq!(mapping.to_value(), wire);
        assert!(mapping.entries().any(|(key, source)| key == "plan"
            && *source == Source::Property(vec!["plan".into(), "tier".into()])));
    }

    #[test]
    fn a_bad_mapping_names_where_and_why() {
        let cases = [
            (json!([]), "", code::NOT_AN_OBJECT),
            (json!({}), "", code::EMPTY),
            (json!({"a b": {"value": 1}}), "/a b", code::INVALID_KEY),
            (json!({"a": "event.id"}), "/a", code::INVALID_SOURCE),
            (
                json!({"a": {"from": "person.email"}}),
                "/a",
                code::UNKNOWN_SOURCE,
            ),
            (
                json!({"a": {"from": "event.id", "pointer": "/x"}}),
                "/a",
                code::INVALID_SOURCE,
            ),
            (
                json!({"a": {"from": "event.properties", "pointer": "plan"}}),
                "/a/pointer",
                code::INVALID_POINTER,
            ),
            (
                json!({"a": {"from": "event.properties", "pointer": "/a//b"}}),
                "/a/pointer",
                code::INVALID_POINTER,
            ),
            (
                json!({"a": {"from": "event.properties", "pointer": "/a/~2"}}),
                "/a/pointer",
                code::INVALID_POINTER,
            ),
            (json!({"a": {"value": [1]}}), "/a", code::INVALID_VALUE),
            (json!({"a": {"value": null}}), "/a", code::INVALID_VALUE),
        ];
        for (wire, pointer, want) in cases {
            let issues = Mapping::parse(&wire).unwrap_err();
            assert_eq!(issues[0], RuleIssue::new(pointer, want), "{wire}");
        }
    }

    #[test]
    fn the_mapping_and_pointer_limits_hold() {
        let many: Map<String, Value> = (0..=MAX_MAPPING_KEYS)
            .map(|n| (format!("k{n}"), json!({"value": 1})))
            .collect();
        assert_eq!(
            Mapping::parse(&Value::Object(many)).unwrap_err()[0].code,
            code::TOO_MANY
        );
        let deep = "/a".repeat(MAX_POINTER_TOKENS + 1);
        assert!(pointer_tokens(&deep).is_none());
        assert_eq!(
            pointer_tokens(&"/a".repeat(MAX_POINTER_TOKENS))
                .unwrap()
                .len(),
            8
        );
        assert_eq!(pointer_tokens("/a~1b/c~0d").unwrap(), ["a/b", "c~d"]);
    }

    #[test]
    fn a_filter_keeps_the_supported_operators_and_refuses_the_rest() {
        let ok = json!([
            {"prop": "plan", "op": "eq", "value": "pro"},
            {"prop": "total", "op": "between", "value": [1, 5]},
            {"prop": "coupon", "op": "exists"},
        ]);
        assert_eq!(parse_filter("order", &ok).unwrap().len(), 3);
        assert_eq!(parse_filter("order", &json!([])).unwrap().len(), 0);

        let relative = json!([{"prop": "at", "op": "within_last", "value": "P1D"}]);
        assert_eq!(
            parse_filter("order", &relative).unwrap_err(),
            [RuleIssue::new("/0/op", code::OP_NOT_SUPPORTED)]
        );
        assert_eq!(
            parse_filter("order", &json!({})).unwrap_err()[0].code,
            code::NOT_AN_OBJECT
        );
        assert_eq!(
            parse_filter("order", &json!([{"prop": "x", "op": "nope"}])).unwrap_err()[0].code,
            code::INVALID_FILTER
        );
    }

    #[test]
    fn the_idempotency_key_is_stable_and_depends_on_both_ids() {
        let (event, destination) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let key = idempotency_key(event, destination);

        assert_eq!(key, idempotency_key(event, destination));
        assert_eq!(key.len(), 64);
        assert_ne!(key, idempotency_key(destination, event));
        assert_ne!(key, idempotency_key(event, Uuid::from_u128(3)));
    }
}
