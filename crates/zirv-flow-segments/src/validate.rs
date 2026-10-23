//! Checks a definition against a catalog (D5). Every issue is collected; the pointer says where
//! and the code says what, and neither carries a value from the definition.

use std::str::FromStr;

use hashbrown::HashMap;
use serde_json::Value;
use zirv_flow_core::consent::{Channel, ConsentStatus, RecordedConsent, TOPIC_DIMENSION};
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::{IdentifierKind, TraitType};
use zirv_flow_core::segment::{
    AggFn, Aggregate, ConsentCondition, Definition, EventCondition, Issue, MAX_DEPTH, MAX_ISSUES,
    MAX_NODES, MessagesCondition, Node, Op, PropertyFilter, TraitCondition, Window, code, parse,
    pointer_push,
};

use crate::catalog::Catalog;
use crate::time_value::{DurationError, parse_date, parse_duration, parse_timestamp};

/// Whether the text has the form of a dimension key or value key (`[a-z0-9_-]{1,64}`).
fn is_dimension_key(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

const MAX_IN_VALUES: usize = 1_000;
const MAX_STRING_CHARS: usize = 1_024;

/// Parses the wire form, then validates it. The parse issues and the validation issues are never
/// mixed: a definition that does not parse is not checked against the catalog.
pub fn parse_and_validate(value: &Value, catalog: &Catalog) -> Result<Definition, Vec<Issue>> {
    parse_and_validate_in(value, catalog, Context::Segment)
}

/// [`parse_and_validate`] for the given context.
pub fn parse_and_validate_in(
    value: &Value,
    catalog: &Catalog,
    context: Context,
) -> Result<Definition, Vec<Issue>> {
    let definition = parse(value)?;
    validate_in(&definition, catalog, context)?;
    Ok(definition)
}

/// Where a definition is used. A consent policy rule reads the message being sent, so it may
/// hold the leaves that need one; a segment may not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Segment,
    Policy,
}

/// Validates a segment definition against the catalog, collecting every issue.
pub fn validate(definition: &Definition, catalog: &Catalog) -> Result<(), Vec<Issue>> {
    validate_in(definition, catalog, Context::Segment)
}

/// Validates a definition for the context that will use it.
pub fn validate_in(
    definition: &Definition,
    catalog: &Catalog,
    context: Context,
) -> Result<(), Vec<Issue>> {
    if definition.v != 1 {
        return Err(vec![Issue::new("/v", code::UNSUPPORTED_VERSION)]);
    }
    let mut checker = Checker {
        catalog,
        context,
        issues: Vec::new(),
        nodes: 0,
    };
    checker.run(&definition.root);
    if checker.issues.is_empty() {
        Ok(())
    } else {
        Err(checker.issues)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    String,
    Number,
    Integer,
    Boolean,
    Timestamp,
    Date,
    StringList,
    Object,
}

impl From<TraitType> for Kind {
    fn from(kind: TraitType) -> Self {
        match kind {
            TraitType::String => Self::String,
            TraitType::Number => Self::Number,
            TraitType::Boolean => Self::Boolean,
            TraitType::Timestamp => Self::Timestamp,
            TraitType::Date => Self::Date,
            TraitType::StringList => Self::StringList,
        }
    }
}

impl From<PropertyType> for Kind {
    fn from(kind: PropertyType) -> Self {
        match kind {
            PropertyType::String => Self::String,
            PropertyType::Number => Self::Number,
            PropertyType::Integer => Self::Integer,
            PropertyType::Boolean => Self::Boolean,
            PropertyType::Timestamp => Self::Timestamp,
            PropertyType::Date => Self::Date,
            PropertyType::StringList => Self::StringList,
            PropertyType::Object => Self::Object,
        }
    }
}

const STRING_OPS: &[Op] = &[
    Op::Eq,
    Op::Neq,
    Op::In,
    Op::NotIn,
    Op::Contains,
    Op::StartsWith,
    Op::Exists,
    Op::NotExists,
];
const NUMBER_OPS: &[Op] = &[
    Op::Eq,
    Op::Neq,
    Op::Gt,
    Op::Gte,
    Op::Lt,
    Op::Lte,
    Op::Between,
    Op::In,
    Op::NotIn,
    Op::Exists,
    Op::NotExists,
];
const BOOLEAN_OPS: &[Op] = &[Op::Eq, Op::Exists, Op::NotExists];
const TIME_OPS: &[Op] = &[
    Op::Before,
    Op::After,
    Op::Between,
    Op::WithinLast,
    Op::MoreThanAgo,
    Op::WithinNext,
    Op::Exists,
    Op::NotExists,
];
const LIST_OPS: &[Op] = &[
    Op::Contains,
    Op::ContainsAll,
    Op::IsEmpty,
    Op::Exists,
    Op::NotExists,
];
const OBJECT_OPS: &[Op] = &[Op::Exists, Op::NotExists];

pub(crate) const fn ops_for(kind: Kind) -> &'static [Op] {
    match kind {
        Kind::String => STRING_OPS,
        Kind::Number | Kind::Integer => NUMBER_OPS,
        Kind::Boolean => BOOLEAN_OPS,
        Kind::Timestamp | Kind::Date => TIME_OPS,
        Kind::StringList => LIST_OPS,
        Kind::Object => OBJECT_OPS,
    }
}

struct Checker<'a> {
    catalog: &'a Catalog,
    context: Context,
    issues: Vec<Issue>,
    nodes: usize,
}

impl Checker<'_> {
    fn report(&mut self, pointer: &str, code: &'static str) {
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(Issue::new(pointer, code));
        }
    }

    /// Walks the tree with an explicit stack and checks the depth before descending.
    fn run(&mut self, root: &Node) {
        let mut stack = vec![(root, String::new(), 1_usize)];
        while let Some((node, pointer, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                self.report(&pointer, code::TOO_DEEP);
                continue;
            }
            if !matches!(node, Node::Not(_)) && !self.count_node(&pointer) {
                return;
            }
            match node {
                Node::All(children) | Node::Any(children) => {
                    let base = pointer_push(&pointer, node.key());
                    for (index, child) in children.iter().enumerate().take(MAX_NODES + 1).rev() {
                        stack.push((child, pointer_push(&base, &index.to_string()), depth + 1));
                    }
                }
                Node::Not(child) => {
                    stack.push((child, pointer_push(&pointer, "not"), depth + 1));
                }
                Node::Trait(condition) => self.trait_condition(condition, &pointer),
                Node::Event(condition) => {
                    if !self.event(condition, &pointer) {
                        return;
                    }
                }
                Node::List(id) => {
                    if !self.catalog.lists.contains(id) {
                        self.report(&pointer_push(&pointer, "list"), code::UNKNOWN_LIST);
                    }
                }
                Node::Segment(id) => {
                    if !self.catalog.segments.contains(id) {
                        self.report(&pointer_push(&pointer, "segment"), code::UNKNOWN_SEGMENT);
                    }
                }
                Node::Consent(condition) => self.consent(condition, &pointer),
                Node::Messages(condition) => self.messages(condition, &pointer),
                Node::Identifier(kind) => {
                    if self.has_nul(kind, &pointer_push(&pointer, "identifier")) {
                        continue;
                    }
                    if IdentifierKind::parse(kind).is_err() {
                        self.report(
                            &pointer_push(&pointer, "identifier"),
                            code::INVALID_IDENTIFIER_KIND,
                        );
                    }
                }
            }
        }
    }

    /// Postgres cannot store NUL in text or jsonb, so a definition string holding one is refused.
    fn has_nul(&mut self, text: &str, pointer: &str) -> bool {
        let found = text.contains('\0');
        if found {
            self.report(pointer, code::VALUE_NOT_ALLOWED);
        }
        found
    }

    /// Counts one node; past the limit it reports once and tells the caller to stop.
    fn count_node(&mut self, pointer: &str) -> bool {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            self.report(pointer, code::TOO_MANY_NODES);
            return false;
        }
        true
    }

    fn trait_condition(&mut self, condition: &TraitCondition, pointer: &str) {
        if self.has_nul(&condition.key, &pointer_push(pointer, "trait")) {
            return;
        }
        match self.catalog.traits.get(&condition.key) {
            Some(kind) => self.condition(
                Kind::from(*kind),
                condition.op,
                condition.value.as_ref(),
                pointer,
                true,
            ),
            None => self.report(&pointer_push(pointer, "trait"), code::UNKNOWN_TRAIT),
        }
    }

    fn consent(&mut self, condition: &ConsentCondition, pointer: &str) {
        let base = pointer_push(pointer, "consent");
        let Some(channel_text) = &condition.channel else {
            self.message_consent(condition, &base);
            return;
        };
        let channel = Channel::from_str(channel_text).ok();
        if channel.is_none() {
            self.report(&pointer_push(&base, "channel"), code::VALUE_NOT_ALLOWED);
        }
        if ConsentStatus::from_str(&condition.status).is_err() {
            self.report(&pointer_push(&base, "status"), code::VALUE_NOT_ALLOWED);
        }
        if let (Some(channel), Some(topic)) = (channel, &condition.topic) {
            let known = self
                .catalog
                .topics
                .get(&channel)
                .is_some_and(|topics| topics.contains_key(topic));
            if !known {
                self.report(&pointer_push(&base, "topic"), code::UNKNOWN_TOPIC);
            }
        }
    }

    /// A consent leaf without a channel reads the consent of the message being sent: a policy
    /// leaf with a `granted`, `denied` or `none` status and no topic of its own.
    fn message_consent(&mut self, condition: &ConsentCondition, base: &str) {
        if self.context != Context::Policy {
            self.report(base, code::POLICY_ONLY);
            return;
        }
        if RecordedConsent::from_name(&condition.status).is_none() {
            self.report(&pointer_push(base, "status"), code::VALUE_NOT_ALLOWED);
        }
        if condition.topic.is_some() {
            self.report(&pointer_push(base, "topic"), code::VALUE_NOT_ALLOWED);
        }
    }

    /// The messages leaf: a count or the time of the last send, counting only sent messages.
    fn messages(&mut self, condition: &MessagesCondition, pointer: &str) {
        let base = pointer_push(pointer, "messages");
        if self.context != Context::Policy {
            self.report(&base, code::POLICY_ONLY);
            return;
        }
        if Channel::from_str(&condition.channel).is_err() {
            self.report(&pointer_push(&base, "channel"), code::VALUE_NOT_ALLOWED);
        }
        let dimensions = pointer_push(&base, "dimensions");
        for (key, value) in &condition.dimensions {
            if !is_dimension_key(key) || key == TOPIC_DIMENSION {
                self.report(&pointer_push(&dimensions, key), code::VALUE_NOT_ALLOWED);
            } else if !is_dimension_key(value) {
                self.report(&pointer_push(&dimensions, key), code::VALUE_NOT_ALLOWED);
            }
        }
        let agg_at = pointer_push(&base, "agg");
        let agg = &condition.agg;
        if agg.prop.is_some() {
            self.report(&pointer_push(&agg_at, "prop"), code::VALUE_NOT_ALLOWED);
        }
        match agg.func {
            AggFn::Count => {
                self.condition(Kind::Number, agg.op, agg.value.as_ref(), &agg_at, false)
            }
            AggFn::LastTime => {
                self.condition(Kind::Timestamp, agg.op, agg.value.as_ref(), &agg_at, false);
            }
            _ => self.report(&pointer_push(&agg_at, "fn"), code::AGGREGATE_NOT_ALLOWED),
        }
        match &condition.window {
            Some(Window::SinceTrait(_)) => {
                self.report(&pointer_push(&base, "window"), code::VALUE_NOT_ALLOWED);
            }
            Some(window) => self.window(window, &pointer_push(&base, "window")),
            None => {}
        }
    }

    /// Returns false when the node budget ran out and the walk must stop.
    fn event(&mut self, condition: &EventCondition, pointer: &str) -> bool {
        let named = !self.has_nul(&condition.name, &pointer_push(pointer, "event"));
        let schema = if named {
            self.catalog.events.get(&condition.name)
        } else {
            None
        };
        if named && schema.is_none() {
            self.report(&pointer_push(pointer, "event"), code::UNKNOWN_EVENT);
        }
        let filters = pointer_push(pointer, "where");
        for (index, filter) in condition.filters.iter().enumerate() {
            let at = pointer_push(&filters, &index.to_string());
            if !self.count_node(&at) {
                return false;
            }
            if let Some(schema) = schema {
                self.filter(filter, schema, &at);
            }
        }
        if let Some(agg) = &condition.agg {
            self.aggregate(agg, schema, &pointer_push(pointer, "agg"));
        }
        if let Some(window) = &condition.window {
            self.window(window, &pointer_push(pointer, "window"));
        }
        true
    }

    fn filter(
        &mut self,
        filter: &PropertyFilter,
        schema: &HashMap<String, PropertyType>,
        pointer: &str,
    ) {
        if self.has_nul(&filter.prop, &pointer_push(pointer, "prop")) {
            return;
        }
        match schema.get(&filter.prop) {
            Some(kind) => self.condition(
                Kind::from(*kind),
                filter.op,
                filter.value.as_ref(),
                pointer,
                true,
            ),
            None => self.report(&pointer_push(pointer, "prop"), code::UNKNOWN_PROPERTY),
        }
    }

    fn aggregate(
        &mut self,
        agg: &Aggregate,
        schema: Option<&HashMap<String, PropertyType>>,
        pointer: &str,
    ) {
        let prop_at = pointer_push(pointer, "prop");
        if agg
            .prop
            .as_deref()
            .is_some_and(|prop| self.has_nul(prop, &prop_at))
        {
            return;
        }
        let kind = match agg.func {
            AggFn::Count | AggFn::FirstTime | AggFn::LastTime => {
                if agg.prop.is_some() {
                    self.report(&prop_at, code::AGGREGATE_NOT_ALLOWED);
                }
                if agg.func == AggFn::Count {
                    Kind::Number
                } else {
                    Kind::Timestamp
                }
            }
            AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::Avg => {
                match agg
                    .prop
                    .as_ref()
                    .map(|prop| schema.map(|schema| schema.get(prop)))
                {
                    None => self.report(&prop_at, code::AGGREGATE_NOT_ALLOWED),
                    Some(None) | Some(Some(Some(PropertyType::Number | PropertyType::Integer))) => {
                    }
                    Some(Some(None)) => self.report(&prop_at, code::UNKNOWN_PROPERTY),
                    Some(Some(Some(_))) => self.report(&prop_at, code::AGGREGATE_NOT_ALLOWED),
                }
                Kind::Number
            }
        };
        self.condition(kind, agg.op, agg.value.as_ref(), pointer, false);
    }

    fn window(&mut self, window: &Window, pointer: &str) {
        match window {
            Window::Last(duration) => {
                self.duration_text(duration, &pointer_push(pointer, "last"));
            }
            Window::Between(start, end) => {
                let at = pointer_push(pointer, "between");
                let start_time = parse_timestamp(start);
                let end_time = parse_timestamp(end);
                if start_time.is_none() {
                    self.report(&pointer_push(&at, "0"), code::VALUE_TYPE_MISMATCH);
                }
                if end_time.is_none() {
                    self.report(&pointer_push(&at, "1"), code::VALUE_TYPE_MISMATCH);
                }
                if let (Some(start), Some(end)) = (start_time, end_time)
                    && start >= end
                {
                    self.report(&at, code::INVALID_RANGE);
                }
            }
            Window::SinceTrait(key) => {
                let at = pointer_push(pointer, "since_trait");
                if self.has_nul(key, &at) {
                    return;
                }
                match self.catalog.traits.get(key) {
                    None => self.report(&at, code::UNKNOWN_TRAIT),
                    Some(TraitType::Timestamp | TraitType::Date) => {}
                    Some(_) => self.report(&at, code::VALUE_TYPE_MISMATCH),
                }
            }
        }
    }

    fn duration_text(&mut self, text: &str, pointer: &str) {
        match parse_duration(text) {
            Ok(_) => {}
            Err(DurationError::Invalid) => self.report(pointer, code::INVALID_DURATION),
            Err(DurationError::TooLong) => self.report(pointer, code::WINDOW_TOO_LONG),
        }
    }

    /// Checks one operator and its value against the value type. `existence` is whether the
    /// presence operators are allowed (they are not on an aggregate, which is never missing).
    fn condition(
        &mut self,
        kind: Kind,
        op: Op,
        value: Option<&Value>,
        pointer: &str,
        existence: bool,
    ) {
        let presence = matches!(op, Op::Exists | Op::NotExists);
        let allowed = ops_for(kind).contains(&op) && (existence || !presence);
        if !allowed {
            self.report(&pointer_push(pointer, "op"), code::OPERATOR_NOT_ALLOWED);
            return;
        }
        let at = pointer_push(pointer, "value");
        if presence || op == Op::IsEmpty {
            if value.is_some() {
                self.report(&at, code::VALUE_NOT_ALLOWED);
            }
            return;
        }
        let Some(value) = value else {
            self.report(&at, code::VALUE_MISSING);
            return;
        };
        match op {
            Op::In | Op::NotIn => self.list(kind, value, &at),
            Op::Contains | Op::ContainsAll if kind == Kind::StringList => {
                self.list(Kind::String, value, &at);
            }
            Op::Between => self.range(kind, value, &at),
            Op::WithinLast | Op::MoreThanAgo | Op::WithinNext => match value.as_str() {
                Some(text) => self.duration_text(text, &at),
                None => self.report(&at, code::VALUE_TYPE_MISMATCH),
            },
            _ => {
                self.scalar(kind, value, &at);
            }
        }
    }

    fn scalar(&mut self, kind: Kind, value: &Value, pointer: &str) -> bool {
        let ok = match kind {
            Kind::String => match value.as_str() {
                Some(text) if text.contains('\0') => {
                    self.report(pointer, code::VALUE_NOT_ALLOWED);
                    return false;
                }
                Some(text) if text.chars().count() > MAX_STRING_CHARS => {
                    self.report(pointer, code::VALUE_TOO_LONG);
                    return false;
                }
                Some(_) => true,
                None => false,
            },
            Kind::Number => value.is_number(),
            Kind::Integer => is_integral(value),
            Kind::Boolean => value.is_boolean(),
            Kind::Timestamp => value
                .as_str()
                .is_some_and(|text| parse_timestamp(text).is_some()),
            Kind::Date => value
                .as_str()
                .is_some_and(|text| parse_date(text).is_some()),
            Kind::StringList | Kind::Object => false,
        };
        if !ok {
            self.report(pointer, code::VALUE_TYPE_MISMATCH);
        }
        ok
    }

    fn list(&mut self, kind: Kind, value: &Value, pointer: &str) {
        let Some(items) = value.as_array() else {
            self.report(pointer, code::VALUE_TYPE_MISMATCH);
            return;
        };
        if items.is_empty() {
            self.report(pointer, code::VALUE_NOT_ALLOWED);
        } else if items.len() > MAX_IN_VALUES {
            self.report(pointer, code::TOO_MANY_VALUES);
        } else {
            for (index, item) in items.iter().enumerate() {
                self.scalar(kind, item, &pointer_push(pointer, &index.to_string()));
            }
        }
    }

    fn range(&mut self, kind: Kind, value: &Value, pointer: &str) {
        let [start, end] = value.as_array().map(Vec::as_slice).unwrap_or_default() else {
            self.report(pointer, code::VALUE_TYPE_MISMATCH);
            return;
        };
        let start_ok = self.scalar(kind, start, &pointer_push(pointer, "0"));
        let end_ok = self.scalar(kind, end, &pointer_push(pointer, "1"));
        if !(start_ok && end_ok) {
            return;
        }
        let ordered = match kind {
            Kind::Timestamp => before(
                start.as_str().and_then(parse_timestamp),
                end.as_str().and_then(parse_timestamp),
                true,
            ),
            Kind::Date => before(
                start.as_str().and_then(parse_date),
                end.as_str().and_then(parse_date),
                true,
            ),
            _ => before(start.as_f64(), end.as_f64(), false),
        };
        if !ordered {
            self.report(pointer, code::INVALID_RANGE);
        }
    }
}

/// Whether `start` precedes `end`; equal ends are allowed unless `strict`.
fn before<T: PartialOrd>(start: Option<T>, end: Option<T>, strict: bool) -> bool {
    match (start, end) {
        (Some(start), Some(end)) if strict => start < end,
        (Some(start), Some(end)) => start <= end,
        _ => false,
    }
}

fn is_integral(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.is_i64()
                || number.is_u64()
                || number
                    .as_f64()
                    .is_some_and(|float| float.is_finite() && float.fract() == 0.0)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    const LIST: &str = "018f0000-0000-7000-8000-000000000001";
    const SEGMENT: &str = "018f0000-0000-7000-8000-000000000002";
    const OTHER: &str = "018f0000-0000-7000-8000-000000000003";
    const NEWS: &str = "018f0000-0000-7000-8000-000000000004";

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        for (key, kind) in [
            ("name", TraitType::String),
            ("age", TraitType::Number),
            ("vip", TraitType::Boolean),
            ("signed_up", TraitType::Timestamp),
            ("birthday", TraitType::Date),
            ("tags", TraitType::StringList),
        ] {
            catalog.traits.insert(key.to_owned(), kind);
        }
        let mut order = HashMap::new();
        for (key, kind) in [
            ("total", PropertyType::Number),
            ("items", PropertyType::Integer),
            ("coupon", PropertyType::String),
            ("gift", PropertyType::Boolean),
            ("shipped_at", PropertyType::Timestamp),
            ("due", PropertyType::Date),
            ("labels", PropertyType::StringList),
            ("extra", PropertyType::Object),
        ] {
            order.insert(key.to_owned(), kind);
        }
        catalog.events.insert("order".to_owned(), order);
        catalog.lists.insert(Uuid::parse_str(LIST).unwrap());
        catalog.segments.insert(Uuid::parse_str(SEGMENT).unwrap());
        let mut topics = HashMap::new();
        topics.insert("news".to_owned(), Uuid::parse_str(NEWS).unwrap());
        catalog.topics.insert(Channel::Email, topics);
        catalog
    }

    fn check(mut value: Value) -> Result<(), Vec<(String, &'static str)>> {
        value["v"] = json!(1);
        let definition = parse(&value).expect("the fixture should parse");
        validate(&definition, &catalog()).map_err(|issues| {
            issues
                .into_iter()
                .map(|issue| (issue.pointer, issue.code))
                .collect()
        })
    }

    fn refused(value: Value, pointer: &str, expected: &'static str) {
        let issues = check(value.clone()).expect_err(&format!("{value} should be refused"));
        assert!(
            issues.contains(&(pointer.to_owned(), expected)),
            "{value}: expected {pointer} {expected}, got {issues:?}"
        );
    }

    fn leaf(key: &str, op: &str, value: Value) -> Value {
        json!({"trait": key, "op": op, "value": value})
    }

    fn event_where(prop: &str, op: &str, value: Value) -> Value {
        json!({"event": "order", "where": [{"prop": prop, "op": op, "value": value}]})
    }

    fn event_agg(func: &str, prop: Option<&str>, op: &str, value: Value) -> Value {
        let mut agg = json!({"fn": func, "op": op, "value": value});
        if let Some(prop) = prop {
            agg["prop"] = json!(prop);
        }
        json!({"event": "order", "agg": agg})
    }

    #[test]
    fn a_version_other_than_one_is_unsupported() {
        let definition = Definition {
            v: 2,
            root: Node::All(vec![]),
        };
        assert_eq!(
            validate(&definition, &catalog()),
            Err(vec![Issue::new("/v", code::UNSUPPORTED_VERSION)])
        );
    }

    #[test]
    fn the_structural_codes_come_from_parsing() {
        let malformed = parse_and_validate(&json!({"v": 1, "all": 3}), &catalog());
        assert_eq!(
            malformed.unwrap_err(),
            vec![Issue::new("/all", code::MALFORMED)]
        );
        let unknown = parse_and_validate(&json!({"v": 1, "all": [], "x": 1}), &catalog());
        assert_eq!(
            unknown.unwrap_err(),
            vec![Issue::new("/x", code::UNKNOWN_KEY)]
        );
        assert!(parse_and_validate(&json!({"v": 1, "all": []}), &catalog()).is_ok());
    }

    #[test]
    fn groups_and_references_validate() {
        let ok = json!({"all": [
            {"any": [{"not": {"list": LIST}}, {"segment": SEGMENT}]},
            {"consent": {"channel": "email", "topic": "news", "status": "subscribed"}},
            {"consent": {"channel": "email", "status": "unknown"}},
            {"identifier": "email"},
            {"identifier": "external:crm.v2"},
        ]});
        assert_eq!(check(ok), Ok(()));
    }

    #[test]
    fn references_to_things_that_do_not_exist_are_refused() {
        refused(
            json!({"all": [{"list": OTHER}]}),
            "/all/0/list",
            code::UNKNOWN_LIST,
        );
        refused(
            json!({"not": {"segment": OTHER}}),
            "/not/segment",
            code::UNKNOWN_SEGMENT,
        );
        refused(
            json!({"consent": {"channel": "email", "topic": "other", "status": "subscribed"}}),
            "/consent/topic",
            code::UNKNOWN_TOPIC,
        );
        refused(
            json!({"consent": {"channel": "sms", "status": "subscribed"}}),
            "/consent/channel",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"consent": {"channel": "email", "status": "maybe"}}),
            "/consent/status",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"identifier": "fax"}),
            "/identifier",
            code::INVALID_IDENTIFIER_KIND,
        );
        refused(
            json!({"identifier": "external:Bad Space"}),
            "/identifier",
            code::INVALID_IDENTIFIER_KIND,
        );
        refused(
            json!({"trait": "nope", "op": "exists"}),
            "/trait",
            code::UNKNOWN_TRAIT,
        );
    }

    #[test]
    fn string_operators() {
        for (op, value) in [
            ("eq", json!("a")),
            ("neq", json!("a")),
            ("in", json!(["a", "b"])),
            ("not_in", json!(["a"])),
            ("contains", json!("a")),
            ("starts_with", json!("a")),
        ] {
            assert_eq!(check(leaf("name", op, value)), Ok(()), "{op}");
        }
        assert_eq!(check(json!({"trait": "name", "op": "exists"})), Ok(()));
        assert_eq!(check(json!({"trait": "name", "op": "not_exists"})), Ok(()));
        refused(
            leaf("name", "gt", json!("a")),
            "/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            leaf("name", "eq", json!(1)),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("name", "in", json!(["a", 1])),
            "/value/1",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("name", "in", json!("a")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn number_operators() {
        for (op, value) in [
            ("eq", json!(1)),
            ("neq", json!(1.5)),
            ("gt", json!(1)),
            ("gte", json!(1)),
            ("lt", json!(1)),
            ("lte", json!(1)),
            ("between", json!([1, 5])),
            ("between", json!([5, 5])),
            ("in", json!([1, 2])),
            ("not_in", json!([1])),
        ] {
            assert_eq!(check(leaf("age", op, value)), Ok(()), "{op}");
        }
        refused(
            leaf("age", "contains", json!(1)),
            "/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            leaf("age", "gt", json!("1")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("age", "between", json!([5, 1])),
            "/value",
            code::INVALID_RANGE,
        );
        refused(
            leaf("age", "between", json!([1])),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("age", "between", json!([1, "x"])),
            "/value/1",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn integer_properties_accept_integral_values_only() {
        assert_eq!(check(event_where("items", "gt", json!(2))), Ok(()));
        assert_eq!(check(event_where("items", "gt", json!(2.0))), Ok(()));
        refused(
            event_where("items", "gt", json!(2.5)),
            "/where/0/value",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn boolean_operators() {
        assert_eq!(check(leaf("vip", "eq", json!(true))), Ok(()));
        assert_eq!(check(json!({"trait": "vip", "op": "exists"})), Ok(()));
        refused(
            leaf("vip", "neq", json!(true)),
            "/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            leaf("vip", "eq", json!("yes")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn timestamp_and_date_operators() {
        for (key, op, value) in [
            ("signed_up", "before", json!("2024-01-01T00:00:00Z")),
            ("signed_up", "after", json!("2024-01-01T00:00:00+02:00")),
            (
                "signed_up",
                "between",
                json!(["2024-01-01T00:00:00Z", "2024-02-01T00:00:00Z"]),
            ),
            ("signed_up", "within_last", json!("P30D")),
            ("signed_up", "more_than_ago", json!("PT12H")),
            ("signed_up", "within_next", json!("P7D")),
            ("birthday", "before", json!("2000-01-01")),
            ("birthday", "between", json!(["2000-01-01", "2000-12-31"])),
            ("birthday", "within_next", json!("P7D")),
        ] {
            assert_eq!(check(leaf(key, op, value)), Ok(()), "{key} {op}");
        }
        refused(
            leaf("signed_up", "eq", json!("2024-01-01T00:00:00Z")),
            "/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            leaf("signed_up", "before", json!("2024-01-01")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("birthday", "before", json!("2000-01-01T00:00:00Z")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf(
                "signed_up",
                "between",
                json!(["2024-02-01T00:00:00Z", "2024-01-01T00:00:00Z"]),
            ),
            "/value",
            code::INVALID_RANGE,
        );
        refused(
            leaf(
                "signed_up",
                "between",
                json!(["2024-01-01T00:00:00Z", "2024-01-01T00:00:00Z"]),
            ),
            "/value",
            code::INVALID_RANGE,
        );
        refused(
            leaf("birthday", "between", json!(["2000-12-31", "2000-01-01"])),
            "/value",
            code::INVALID_RANGE,
        );
    }

    #[test]
    fn relative_durations_are_checked() {
        refused(
            leaf("signed_up", "within_last", json!("30 days")),
            "/value",
            code::INVALID_DURATION,
        );
        refused(
            leaf("signed_up", "within_last", json!("PT30M")),
            "/value",
            code::INVALID_DURATION,
        );
        refused(
            leaf("signed_up", "within_last", json!("P1096D")),
            "/value",
            code::WINDOW_TOO_LONG,
        );
        refused(
            leaf("signed_up", "within_last", json!(30)),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn string_list_operators() {
        assert_eq!(check(leaf("tags", "contains", json!(["a", "b"]))), Ok(()));
        assert_eq!(check(leaf("tags", "contains_all", json!(["a"]))), Ok(()));
        assert_eq!(check(json!({"trait": "tags", "op": "is_empty"})), Ok(()));
        assert_eq!(check(json!({"trait": "tags", "op": "exists"})), Ok(()));
        refused(
            leaf("tags", "eq", json!(["a"])),
            "/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            leaf("tags", "contains", json!("a")),
            "/value",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            leaf("tags", "contains", json!([1])),
            "/value/0",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn object_properties_only_have_existence_operators() {
        assert_eq!(
            check(json!({"event": "order", "where": [{"prop": "extra", "op": "exists"}]})),
            Ok(())
        );
        refused(
            event_where("extra", "eq", json!({})),
            "/where/0/op",
            code::OPERATOR_NOT_ALLOWED,
        );
    }

    #[test]
    fn a_value_is_required_or_forbidden_by_the_operator() {
        refused(
            json!({"trait": "name", "op": "eq"}),
            "/value",
            code::VALUE_MISSING,
        );
        refused(
            json!({"trait": "name", "op": "exists", "value": 1}),
            "/value",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"trait": "tags", "op": "is_empty", "value": []}),
            "/value",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            leaf("name", "in", json!([])),
            "/value",
            code::VALUE_NOT_ALLOWED,
        );
    }

    #[test]
    fn value_size_limits() {
        let many: Vec<Value> = (0..1_000).map(|n| json!(n)).collect();
        assert_eq!(check(leaf("age", "in", Value::Array(many.clone()))), Ok(()));
        let mut too_many = many;
        too_many.push(json!(1_000));
        refused(
            leaf("age", "in", Value::Array(too_many)),
            "/value",
            code::TOO_MANY_VALUES,
        );
        assert_eq!(check(leaf("name", "eq", json!("a".repeat(1_024)))), Ok(()));
        refused(
            leaf("name", "eq", json!("a".repeat(1_025))),
            "/value",
            code::VALUE_TOO_LONG,
        );
        refused(
            leaf("name", "in", json!(["a", "b".repeat(1_025)])),
            "/value/1",
            code::VALUE_TOO_LONG,
        );
    }

    #[test]
    fn events_and_properties_must_exist() {
        refused(json!({"event": "nope"}), "/event", code::UNKNOWN_EVENT);
        refused(
            json!({"event": "order", "where": [{"prop": "nope", "op": "exists"}]}),
            "/where/0/prop",
            code::UNKNOWN_PROPERTY,
        );
        assert_eq!(check(json!({"event": "order"})), Ok(()));
        assert_eq!(
            check(json!({"event": "order", "where": [
                {"prop": "coupon", "op": "starts_with", "value": "SUM"},
                {"prop": "gift", "op": "eq", "value": true},
                {"prop": "shipped_at", "op": "within_last", "value": "P7D"},
                {"prop": "due", "op": "after", "value": "2024-01-01"},
                {"prop": "labels", "op": "contains_all", "value": ["x"]},
            ]})),
            Ok(())
        );
    }

    #[test]
    fn aggregates() {
        for (func, prop, op, value) in [
            ("count", None, "gte", json!(3)),
            ("sum", Some("total"), "gt", json!(100.5)),
            ("min", Some("items"), "lt", json!(2)),
            ("max", Some("total"), "between", json!([1, 2])),
            ("avg", Some("total"), "gte", json!(10)),
            ("first_time", None, "after", json!("2024-01-01T00:00:00Z")),
            ("last_time", None, "within_last", json!("P30D")),
        ] {
            assert_eq!(
                check(event_agg(func, prop, op, value)),
                Ok(()),
                "{func} {op}"
            );
        }
        refused(
            event_agg("count", Some("total"), "gt", json!(1)),
            "/agg/prop",
            code::AGGREGATE_NOT_ALLOWED,
        );
        refused(
            event_agg("sum", None, "gt", json!(1)),
            "/agg/prop",
            code::AGGREGATE_NOT_ALLOWED,
        );
        refused(
            event_agg("sum", Some("coupon"), "gt", json!(1)),
            "/agg/prop",
            code::AGGREGATE_NOT_ALLOWED,
        );
        refused(
            event_agg("sum", Some("nope"), "gt", json!(1)),
            "/agg/prop",
            code::UNKNOWN_PROPERTY,
        );
        refused(
            event_agg(
                "first_time",
                Some("shipped_at"),
                "after",
                json!("2024-01-01T00:00:00Z"),
            ),
            "/agg/prop",
            code::AGGREGATE_NOT_ALLOWED,
        );
        refused(
            json!({"event": "order", "agg": {"fn": "count", "op": "exists"}}),
            "/agg/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            event_agg("count", None, "contains", json!(1)),
            "/agg/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            event_agg("last_time", None, "gt", json!(1)),
            "/agg/op",
            code::OPERATOR_NOT_ALLOWED,
        );
        refused(
            json!({"event": "order", "agg": {"fn": "count", "op": "gt"}}),
            "/agg/value",
            code::VALUE_MISSING,
        );
        refused(
            event_agg("count", None, "gt", json!("1")),
            "/agg/value",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn windows() {
        for window in [
            json!({"last": "P90D"}),
            json!({"last": "PT1H"}),
            json!({"last": "P1095D"}),
            json!({"between": ["2024-01-01T00:00:00Z", "2024-02-01T00:00:00Z"]}),
            json!({"since_trait": "signed_up"}),
            json!({"since_trait": "birthday"}),
        ] {
            assert_eq!(check(json!({"event": "order", "window": window})), Ok(()));
        }
        refused(
            json!({"event": "order", "window": {"last": "P1096D"}}),
            "/window/last",
            code::WINDOW_TOO_LONG,
        );
        refused(
            json!({"event": "order", "window": {"last": "PT0H"}}),
            "/window/last",
            code::INVALID_DURATION,
        );
        refused(
            json!({"event": "order", "window": {"last": "soon"}}),
            "/window/last",
            code::INVALID_DURATION,
        );
        refused(
            json!({"event": "order", "window": {"between": ["2024-02-01T00:00:00Z", "2024-01-01T00:00:00Z"]}}),
            "/window/between",
            code::INVALID_RANGE,
        );
        refused(
            json!({"event": "order", "window": {"between": ["2024-01-01T00:00:00Z", "2024-01-01T00:00:00Z"]}}),
            "/window/between",
            code::INVALID_RANGE,
        );
        refused(
            json!({"event": "order", "window": {"between": ["2024-01-01", "2024-02-01T00:00:00Z"]}}),
            "/window/between/0",
            code::VALUE_TYPE_MISMATCH,
        );
        refused(
            json!({"event": "order", "window": {"since_trait": "nope"}}),
            "/window/since_trait",
            code::UNKNOWN_TRAIT,
        );
        refused(
            json!({"event": "order", "window": {"since_trait": "name"}}),
            "/window/since_trait",
            code::VALUE_TYPE_MISMATCH,
        );
    }

    #[test]
    fn an_unknown_event_still_checks_the_aggregate_parts_that_need_no_schema() {
        let issues =
            check(json!({"event": "missing", "agg": {"fn": "count", "op": "exists"}})).unwrap_err();
        assert_eq!(
            issues,
            [
                ("/event".to_owned(), code::UNKNOWN_EVENT),
                ("/agg/op".to_owned(), code::OPERATOR_NOT_ALLOWED)
            ]
        );
        refused(
            json!({"event": "missing", "agg": {"fn": "sum", "op": "gt", "value": 1}}),
            "/agg/prop",
            code::AGGREGATE_NOT_ALLOWED,
        );
        refused(
            json!({"event": "missing", "agg": {"fn": "count", "op": "gt"}}),
            "/agg/value",
            code::VALUE_MISSING,
        );
        assert_eq!(
            check(json!({"event": "missing", "agg": {"fn": "sum", "prop": "x", "op": "gt", "value": 1}}))
                .unwrap_err(),
            [("/event".to_owned(), code::UNKNOWN_EVENT)]
        );
    }

    #[test]
    fn an_unknown_event_still_checks_its_window() {
        let issues = check(json!({"event": "nope", "window": {"last": "P1096D"}})).unwrap_err();
        assert_eq!(
            issues,
            [
                ("/event".to_owned(), code::UNKNOWN_EVENT),
                ("/window/last".to_owned(), code::WINDOW_TOO_LONG)
            ]
        );
    }

    #[test]
    fn all_issues_are_collected_with_their_pointers() {
        let issues = check(json!({"all": [
            {"list": OTHER},
            {"trait": "nope", "op": "exists"},
            {"not": leaf("age", "eq", json!("x"))},
        ]}))
        .unwrap_err();
        assert_eq!(
            issues,
            [
                ("/all/0/list".to_owned(), code::UNKNOWN_LIST),
                ("/all/1/trait".to_owned(), code::UNKNOWN_TRAIT),
                ("/all/2/not/value".to_owned(), code::VALUE_TYPE_MISMATCH),
            ]
        );
    }

    #[test]
    fn issues_never_carry_a_value_from_the_definition() {
        let secret = "s3cret-value";
        let issues = check(json!({"all": [
            {"trait": secret, "op": "exists"},
            leaf("age", "eq", json!(secret)),
        ]}))
        .unwrap_err();
        assert!(!format!("{issues:?}").contains(secret));
    }

    #[test]
    fn nesting_past_the_depth_limit_is_refused_without_recursing() {
        let deep = |levels: usize| {
            let mut node = Node::All(vec![]);
            for _ in 1..levels {
                node = Node::Not(Box::new(node));
            }
            Definition { v: 1, root: node }
        };
        assert_eq!(validate(&deep(MAX_DEPTH), &catalog()), Ok(()));
        let definition = deep(MAX_DEPTH + 1);
        let issues = validate(&definition, &catalog()).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, code::TOO_DEEP);
        assert_eq!(issues[0].pointer.matches("/not").count(), MAX_DEPTH);
    }

    fn counted(node: &Node) -> usize {
        match node {
            Node::All(children) | Node::Any(children) => {
                1 + children.iter().map(counted).sum::<usize>()
            }
            Node::Not(child) => counted(child),
            Node::Event(condition) => 1 + condition.filters.len(),
            _ => 1,
        }
    }

    fn leaves(count: usize) -> Vec<Node> {
        (0..count)
            .map(|index| {
                Node::Trait(TraitCondition {
                    key: "age".into(),
                    op: Op::Eq,
                    value: Some(json!(index)),
                })
            })
            .collect()
    }

    #[test]
    fn a_not_over_a_near_limit_group_still_validates_after_normalizing() {
        let definition = Definition {
            v: 1,
            root: Node::Not(Box::new(Node::All(leaves(MAX_NODES - 2)))),
        };
        assert_eq!(counted(&definition.root), MAX_NODES - 1);
        assert_eq!(validate(&definition, &catalog()), Ok(()));
        let normalized = crate::normalize(definition);
        assert_eq!(validate(&normalized, &catalog()), Ok(()));
    }

    #[test]
    fn normalizing_never_increases_the_counted_nodes() {
        let leaf = || leaves(1).remove(0);
        let shapes = [
            Node::Not(Box::new(Node::All(leaves(30)))),
            Node::Not(Box::new(Node::Any(vec![
                Node::All(leaves(5)),
                Node::Not(Box::new(Node::Any(leaves(7)))),
            ]))),
            Node::All(vec![
                Node::Not(Box::new(Node::All(vec![leaf(), Node::Any(leaves(4))]))),
                Node::Any(vec![Node::Any(leaves(3)), Node::All(vec![leaf()])]),
            ]),
            Node::Not(Box::new(Node::Not(Box::new(Node::Not(Box::new(leaf())))))),
            Node::Not(Box::new(Node::All(vec![Node::Any(vec![])]))),
        ];
        for root in shapes {
            let before = counted(&root);
            let after = counted(&crate::normalize(Definition { v: 1, root }).root);
            assert!(after <= before, "{after} > {before}");
        }
    }

    #[test]
    fn more_nodes_than_the_limit_are_refused_once() {
        let build = |count: usize| Definition {
            v: 1,
            root: Node::All((0..count).map(|_| Node::Any(vec![])).collect()),
        };
        assert_eq!(validate(&build(MAX_NODES - 1), &catalog()), Ok(()));
        let issues = validate(&build(MAX_NODES + 5_000), &catalog()).unwrap_err();
        assert_eq!(
            issues,
            [Issue::new(
                format!("/all/{}", MAX_NODES - 1),
                code::TOO_MANY_NODES
            )]
        );
    }

    #[test]
    fn where_filters_count_toward_the_node_limit() {
        let filter = PropertyFilter {
            prop: "total".into(),
            op: Op::Exists,
            value: None,
        };
        let definition = Definition {
            v: 1,
            root: Node::Event(EventCondition {
                name: "order".into(),
                filters: vec![filter; MAX_NODES],
                agg: None,
                window: None,
            }),
        };
        let issues = validate(&definition, &catalog()).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, code::TOO_MANY_NODES);
    }

    #[test]
    fn a_nul_in_any_definition_string_is_refused() {
        let nul = "a\u{0}b";
        refused(
            leaf("name", "eq", json!(nul)),
            "/value",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            leaf("name", "in", json!(["ok", nul])),
            "/value/1",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            leaf("name", "contains", json!(nul)),
            "/value",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            leaf("tags", "contains", json!([nul])),
            "/value/0",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"trait": nul, "op": "exists"}),
            "/trait",
            code::VALUE_NOT_ALLOWED,
        );
        refused(json!({"event": nul}), "/event", code::VALUE_NOT_ALLOWED);
        refused(
            json!({"event": "order", "where": [{"prop": nul, "op": "exists"}]}),
            "/where/0/prop",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            event_where("coupon", "eq", json!(nul)),
            "/where/0/value",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            event_agg("sum", Some(nul), "gt", json!(1)),
            "/agg/prop",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"event": "order", "window": {"since_trait": nul}}),
            "/window/since_trait",
            code::VALUE_NOT_ALLOWED,
        );
        refused(
            json!({"identifier": format!("external:{nul}")}),
            "/identifier",
            code::VALUE_NOT_ALLOWED,
        );
    }

    fn check_policy(mut value: Value) -> Result<(), Vec<(String, &'static str)>> {
        value["v"] = json!(1);
        let definition = parse(&value).expect("the fixture should parse");
        validate_in(&definition, &catalog(), Context::Policy).map_err(|issues| {
            issues
                .into_iter()
                .map(|issue| (issue.pointer, issue.code))
                .collect()
        })
    }

    #[test]
    fn the_policy_leaves_are_refused_in_a_segment_with_a_clear_code() {
        for value in [
            json!({"consent": {"status": "granted"}}),
            json!({"messages": {"channel": "email",
                "agg": {"fn": "count", "op": "gte", "value": 1}}}),
        ] {
            let key = if value.get("consent").is_some() {
                "consent"
            } else {
                "messages"
            };
            refused(value, &format!("/{key}"), code::POLICY_ONLY);
        }
    }

    #[test]
    fn the_policy_leaves_are_accepted_in_a_policy() {
        let accepted = [
            json!({"consent": {"status": "none"}}),
            json!({"consent": {"channel": "email", "status": "subscribed"}}),
            json!({"messages": {"channel": "email", "dimensions": {"vertical": "sports"},
                "agg": {"fn": "count", "op": "gte", "value": 3}, "window": {"last": "P7D"}}}),
            json!({"messages": {"channel": "email",
                "agg": {"fn": "last_time", "op": "within_last", "value": "P1D"}}}),
        ];
        for value in accepted {
            assert_eq!(check_policy(value.clone()), Ok(()), "{value}");
        }
    }

    #[test]
    fn a_policy_consent_leaf_takes_only_the_three_message_statuses_and_no_topic() {
        let issues = check_policy(json!({"consent": {"status": "subscribed"}})).unwrap_err();
        assert_eq!(
            issues,
            [("/consent/status".to_owned(), code::VALUE_NOT_ALLOWED)]
        );
        let issues =
            check_policy(json!({"consent": {"status": "denied", "topic": "news"}})).unwrap_err();
        assert_eq!(
            issues,
            [("/consent/topic".to_owned(), code::VALUE_NOT_ALLOWED)]
        );
        let issues = check_policy(json!({"consent": {"channel": "email", "status": "granted"}}))
            .unwrap_err();
        assert_eq!(
            issues,
            [("/consent/status".to_owned(), code::VALUE_NOT_ALLOWED)]
        );
    }

    #[test]
    fn a_messages_leaf_is_checked_for_channel_dimensions_function_and_window() {
        let bad = |value: Value| check_policy(value).unwrap_err();
        assert_eq!(
            bad(json!({"messages": {"channel": "fax",
                "agg": {"fn": "count", "op": "gte", "value": 1}}})),
            [("/messages/channel".to_owned(), code::VALUE_NOT_ALLOWED)]
        );
        assert_eq!(
            bad(
                json!({"messages": {"channel": "email", "dimensions": {"topic": "news"},
                "agg": {"fn": "count", "op": "gte", "value": 1}}})
            ),
            [(
                "/messages/dimensions/topic".to_owned(),
                code::VALUE_NOT_ALLOWED
            )]
        );
        assert_eq!(
            bad(
                json!({"messages": {"channel": "email", "dimensions": {"Vertical": "x"},
                "agg": {"fn": "count", "op": "gte", "value": 1}}})
            ),
            [(
                "/messages/dimensions/Vertical".to_owned(),
                code::VALUE_NOT_ALLOWED
            )]
        );
        assert_eq!(
            bad(json!({"messages": {"channel": "email",
                "agg": {"fn": "sum", "prop": "x", "op": "gte", "value": 1}}})),
            [
                ("/messages/agg/prop".to_owned(), code::VALUE_NOT_ALLOWED),
                ("/messages/agg/fn".to_owned(), code::AGGREGATE_NOT_ALLOWED),
            ]
        );
        assert_eq!(
            bad(
                json!({"messages": {"channel": "email", "window": {"since_trait": "signed_up"},
                "agg": {"fn": "count", "op": "gte", "value": 1}}})
            ),
            [("/messages/window".to_owned(), code::VALUE_NOT_ALLOWED)]
        );
        assert_eq!(
            bad(
                json!({"messages": {"channel": "email", "window": {"last": "P5000D"},
                "agg": {"fn": "count", "op": "gte", "value": 1}}})
            ),
            [("/messages/window/last".to_owned(), code::WINDOW_TOO_LONG)]
        );
        assert_eq!(
            bad(json!({"messages": {"channel": "email",
                "agg": {"fn": "count", "op": "within_last", "value": "P1D"}}})),
            [("/messages/agg/op".to_owned(), code::OPERATOR_NOT_ALLOWED)]
        );
    }
}
