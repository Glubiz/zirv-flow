//! The AST types and their wire form (D2). Durations and timestamps stay text here; the
//! validator checks them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

use serde_json::{Map, Value};
use uuid::Uuid;

use super::issue::Issue;
use super::parse::parse;

/// The key that names each node kind on the wire.
pub const NODE_KEYS: [&str; 10] = [
    "all",
    "any",
    "not",
    "trait",
    "event",
    "list",
    "segment",
    "consent",
    "identifier",
    "messages",
];

/// A comparison operator. Which ones a value type accepts is the validator's concern.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Op {
    Eq,
    Neq,
    In,
    NotIn,
    Contains,
    StartsWith,
    Exists,
    NotExists,
    Gt,
    Gte,
    Lt,
    Lte,
    Between,
    Before,
    After,
    WithinLast,
    MoreThanAgo,
    WithinNext,
    ContainsAll,
    IsEmpty,
}

impl Op {
    pub const ALL: [Op; 20] = [
        Self::Eq,
        Self::Neq,
        Self::In,
        Self::NotIn,
        Self::Contains,
        Self::StartsWith,
        Self::Exists,
        Self::NotExists,
        Self::Gt,
        Self::Gte,
        Self::Lt,
        Self::Lte,
        Self::Between,
        Self::Before,
        Self::After,
        Self::WithinLast,
        Self::MoreThanAgo,
        Self::WithinNext,
        Self::ContainsAll,
        Self::IsEmpty,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Neq => "neq",
            Self::In => "in",
            Self::NotIn => "not_in",
            Self::Contains => "contains",
            Self::StartsWith => "starts_with",
            Self::Exists => "exists",
            Self::NotExists => "not_exists",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Between => "between",
            Self::Before => "before",
            Self::After => "after",
            Self::WithinLast => "within_last",
            Self::MoreThanAgo => "more_than_ago",
            Self::WithinNext => "within_next",
            Self::ContainsAll => "contains_all",
            Self::IsEmpty => "is_empty",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.as_str() == name)
    }

    /// Whether the op depends on the current time (a relative duration).
    pub const fn is_relative(self) -> bool {
        matches!(
            self,
            Self::WithinLast | Self::MoreThanAgo | Self::WithinNext
        )
    }
}

/// An aggregate function over the matching events.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AggFn {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    FirstTime,
    LastTime,
}

impl AggFn {
    pub const ALL: [AggFn; 7] = [
        Self::Count,
        Self::Sum,
        Self::Min,
        Self::Max,
        Self::Avg,
        Self::FirstTime,
        Self::LastTime,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
            Self::FirstTime => "first_time",
            Self::LastTime => "last_time",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|func| func.as_str() == name)
    }
}

/// A segment definition: the version and the root node.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub v: u64,
    pub root: Node,
}

/// One node of the condition tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
    Trait(TraitCondition),
    Event(EventCondition),
    List(Uuid),
    Segment(Uuid),
    Consent(ConsentCondition),
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    Identifier(String),
    /// A consent policy leaf: the messages already sent. Refused in a segment.
    Messages(MessagesCondition),
}

impl Node {
    /// The wire key of this node kind; one of [`NODE_KEYS`].
    pub const fn key(&self) -> &'static str {
        match self {
            Self::All(_) => "all",
            Self::Any(_) => "any",
            Self::Not(_) => "not",
            Self::Trait(_) => "trait",
            Self::Event(_) => "event",
            Self::List(_) => "list",
            Self::Segment(_) => "segment",
            Self::Consent(_) => "consent",
            Self::Identifier(_) => "identifier",
            Self::Messages(_) => "messages",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraitCondition {
    pub key: String,
    pub op: Op,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropertyFilter {
    pub prop: String,
    pub op: Op,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aggregate {
    pub func: AggFn,
    pub prop: Option<String>,
    pub op: Op,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Window {
    /// An ISO 8601 duration in days or hours.
    Last(String),
    /// RFC 3339 start and end.
    Between(String, String),
    /// A timestamp or date trait key.
    SinceTrait(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventCondition {
    pub name: String,
    pub filters: Vec<PropertyFilter>,
    pub agg: Option<Aggregate>,
    pub window: Option<Window>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConsentCondition {
    /// A segment names the channel. A consent policy leaf leaves it out and means the message's
    /// own channel and dimension values, with a `granted`, `denied` or `none` status.
    pub channel: Option<String>,
    pub topic: Option<String>,
    pub status: String,
}

/// The messages a person was sent (architecture 10.1): a count, or the time of the last one, on a
/// channel within a window, counting only messages that were sent. Valid in a consent policy only.
#[derive(Clone, Debug, PartialEq)]
pub struct MessagesCondition {
    pub channel: String,
    /// Dimension key to value key; a message must carry all of them.
    pub dimensions: BTreeMap<String, String>,
    /// `count` or `last_time`, with `prop` unset.
    pub agg: Aggregate,
    /// Only `last` and `between` are valid.
    pub window: Option<Window>,
}

fn put_condition(map: &mut Map<String, Value>, op: Op, value: &Option<Value>) {
    map.insert("op".to_owned(), Value::String(op.as_str().to_owned()));
    if let Some(value) = value {
        map.insert("value".to_owned(), value.clone());
    }
}

impl Window {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::Last(duration) => map.insert("last".to_owned(), duration.clone().into()),
            Self::Between(start, end) => map.insert(
                "between".to_owned(),
                Value::Array(vec![start.clone().into(), end.clone().into()]),
            ),
            Self::SinceTrait(key) => map.insert("since_trait".to_owned(), key.clone().into()),
        };
        Value::Object(map)
    }
}

impl Node {
    /// The wire form of this node.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::All(children) | Self::Any(children) => {
                let items = children.iter().map(Node::to_value).collect();
                map.insert(self.key().to_owned(), Value::Array(items));
            }
            Self::Not(child) => {
                map.insert("not".to_owned(), child.to_value());
            }
            Self::Trait(condition) => {
                map.insert("trait".to_owned(), condition.key.clone().into());
                put_condition(&mut map, condition.op, &condition.value);
            }
            Self::Event(condition) => {
                map.insert("event".to_owned(), condition.name.clone().into());
                if !condition.filters.is_empty() {
                    let filters = condition
                        .filters
                        .iter()
                        .map(|filter| {
                            let mut item = Map::new();
                            item.insert("prop".to_owned(), filter.prop.clone().into());
                            put_condition(&mut item, filter.op, &filter.value);
                            Value::Object(item)
                        })
                        .collect();
                    map.insert("where".to_owned(), Value::Array(filters));
                }
                if let Some(agg) = &condition.agg {
                    let mut item = Map::new();
                    item.insert("fn".to_owned(), agg.func.as_str().into());
                    if let Some(prop) = &agg.prop {
                        item.insert("prop".to_owned(), prop.clone().into());
                    }
                    put_condition(&mut item, agg.op, &agg.value);
                    map.insert("agg".to_owned(), Value::Object(item));
                }
                if let Some(window) = &condition.window {
                    map.insert("window".to_owned(), window.to_value());
                }
            }
            Self::List(id) => {
                map.insert("list".to_owned(), id.to_string().into());
            }
            Self::Segment(id) => {
                map.insert("segment".to_owned(), id.to_string().into());
            }
            Self::Consent(condition) => {
                let mut item = Map::new();
                if let Some(channel) = &condition.channel {
                    item.insert("channel".to_owned(), channel.clone().into());
                }
                if let Some(topic) = &condition.topic {
                    item.insert("topic".to_owned(), topic.clone().into());
                }
                item.insert("status".to_owned(), condition.status.clone().into());
                map.insert("consent".to_owned(), Value::Object(item));
            }
            Self::Identifier(kind) => {
                map.insert("identifier".to_owned(), kind.clone().into());
            }
            Self::Messages(condition) => {
                let mut item = Map::new();
                item.insert("channel".to_owned(), condition.channel.clone().into());
                if !condition.dimensions.is_empty() {
                    let dimensions = condition
                        .dimensions
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                        .collect();
                    item.insert("dimensions".to_owned(), Value::Object(dimensions));
                }
                let mut agg = Map::new();
                agg.insert("fn".to_owned(), condition.agg.func.as_str().into());
                put_condition(&mut agg, condition.agg.op, &condition.agg.value);
                item.insert("agg".to_owned(), Value::Object(agg));
                if let Some(window) = &condition.window {
                    item.insert("window".to_owned(), window.to_value());
                }
                map.insert("messages".to_owned(), Value::Object(item));
            }
        }
        Value::Object(map)
    }
}

impl Definition {
    /// The wire form: the root node with `"v"` added.
    pub fn to_value(&self) -> Value {
        let mut value = self.root.to_value();
        if let Value::Object(map) = &mut value {
            map.insert("v".to_owned(), self.v.into());
        }
        value
    }

    /// Parses the wire form, reporting every structural problem with a pointer.
    pub fn from_value(value: &Value) -> Result<Self, Vec<Issue>> {
        parse(value)
    }
}

impl Serialize for Definition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl Serialize for Node {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        parse(&value).map_err(|issues| {
            let first = issues.first().map(Issue::to_string).unwrap_or_default();
            serde::de::Error::custom(format!("invalid segment definition ({first})"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_and_function_round_trips_through_its_name() {
        for op in Op::ALL {
            assert_eq!(Op::from_name(op.as_str()), Some(op));
        }
        for func in AggFn::ALL {
            assert_eq!(AggFn::from_name(func.as_str()), Some(func));
        }
        assert_eq!(Op::from_name("EQ"), None);
        assert_eq!(AggFn::from_name("median"), None);
    }

    #[test]
    fn the_node_keys_match_the_variants() {
        let id = Uuid::nil();
        let nodes = [
            Node::All(vec![]),
            Node::Any(vec![]),
            Node::Not(Box::new(Node::All(vec![]))),
            Node::Trait(TraitCondition {
                key: "k".into(),
                op: Op::Exists,
                value: None,
            }),
            Node::Event(EventCondition {
                name: "e".into(),
                filters: vec![],
                agg: None,
                window: None,
            }),
            Node::List(id),
            Node::Segment(id),
            Node::Consent(ConsentCondition {
                channel: Some("email".into()),
                topic: None,
                status: "subscribed".into(),
            }),
            Node::Identifier("email".into()),
            Node::Messages(MessagesCondition {
                channel: "email".into(),
                dimensions: BTreeMap::new(),
                agg: Aggregate {
                    func: AggFn::Count,
                    prop: None,
                    op: Op::Gte,
                    value: Some(Value::from(1)),
                },
                window: None,
            }),
        ];
        let keys: Vec<&str> = nodes.iter().map(Node::key).collect();
        assert_eq!(keys, NODE_KEYS);
    }

    #[test]
    fn a_definition_serializes_with_its_version_beside_the_root_key() {
        let definition = Definition {
            v: 1,
            root: Node::Not(Box::new(Node::Identifier("email".into()))),
        };
        assert_eq!(
            serde_json::to_value(&definition).unwrap(),
            serde_json::json!({"v": 1, "not": {"identifier": "email"}})
        );
    }
}
