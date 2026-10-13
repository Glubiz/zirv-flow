//! Reads the wire form into the AST. Every structural problem is reported with a JSON pointer;
//! the depth and node limits are checked before descending, so a hostile definition cannot
//! exhaust the stack.

use serde_json::{Map, Value};
use uuid::Uuid;

use super::ast::{
    AggFn, Aggregate, ConsentCondition, Definition, EventCondition, NODE_KEYS, Node, Op,
    PropertyFilter, TraitCondition, Window,
};
use super::issue::{Issue, code, pointer_push};

/// The deepest nesting of nodes; the root is depth 1.
pub const MAX_DEPTH: usize = 10;
/// The most nodes in one definition. An event's `where` filters count as nodes.
pub const MAX_NODES: usize = 200;
/// The most issues one pass reports; the rest are dropped.
pub const MAX_ISSUES: usize = 200;

/// Parses a definition, collecting every structural issue.
pub fn parse(value: &Value) -> Result<Definition, Vec<Issue>> {
    let Some(map) = value.as_object() else {
        return Err(vec![Issue::new("", code::MALFORMED)]);
    };
    let mut parser = Parser::default();
    let version = match map.get("v").and_then(Value::as_u64) {
        Some(version) => Some(version),
        None => {
            parser.report("/v", code::MALFORMED);
            None
        }
    };
    let root = parser.node(map, "", 1, true);
    match (parser.issues.is_empty(), version, root) {
        (true, Some(v), Some(root)) => Ok(Definition { v, root }),
        _ => Err(parser.issues),
    }
}

#[derive(Default)]
struct Parser {
    issues: Vec<Issue>,
    nodes: usize,
    halted: bool,
}

impl Parser {
    fn report(&mut self, pointer: &str, code: &'static str) {
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(Issue::new(pointer, code));
        }
    }

    fn count_node(&mut self, pointer: &str) -> bool {
        if self.halted {
            return false;
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            self.halted = true;
            self.report(pointer, code::TOO_MANY_NODES);
            return false;
        }
        true
    }

    fn unknown_keys(&mut self, map: &Map<String, Value>, pointer: &str, allowed: &[&str]) {
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                self.report(&pointer_push(pointer, key), code::UNKNOWN_KEY);
            }
        }
    }

    fn string(&mut self, value: &Value, pointer: &str) -> Option<String> {
        match value.as_str() {
            Some(text) => Some(text.to_owned()),
            None => {
                self.report(pointer, code::MALFORMED);
                None
            }
        }
    }

    fn required_string(
        &mut self,
        map: &Map<String, Value>,
        key: &str,
        pointer: &str,
    ) -> Option<String> {
        let at = pointer_push(pointer, key);
        match map.get(key) {
            Some(value) => self.string(value, &at),
            None => {
                self.report(&at, code::MALFORMED);
                None
            }
        }
    }

    fn op_value(&mut self, map: &Map<String, Value>, pointer: &str) -> Option<(Op, Option<Value>)> {
        let at = pointer_push(pointer, "op");
        let op = match map.get("op") {
            None => {
                self.report(&at, code::MALFORMED);
                None
            }
            Some(Value::String(name)) => {
                let op = Op::from_name(name);
                if op.is_none() {
                    self.report(&at, code::OPERATOR_NOT_ALLOWED);
                }
                op
            }
            Some(_) => {
                self.report(&at, code::MALFORMED);
                None
            }
        };
        op.map(|op| (op, map.get("value").cloned()))
    }

    fn node(
        &mut self,
        map: &Map<String, Value>,
        pointer: &str,
        depth: usize,
        root: bool,
    ) -> Option<Node> {
        if depth > MAX_DEPTH {
            if !self.halted {
                self.report(pointer, code::TOO_DEEP);
            }
            return None;
        }
        if !self.count_node(pointer) {
            return None;
        }
        let present: Vec<&str> = NODE_KEYS
            .into_iter()
            .filter(|key| map.contains_key(*key))
            .collect();
        let [key] = present[..] else {
            self.report(pointer, code::MALFORMED);
            for name in map.keys() {
                let known = NODE_KEYS.contains(&name.as_str()) || (root && name == "v");
                if !known {
                    self.report(&pointer_push(pointer, name), code::UNKNOWN_KEY);
                }
            }
            return None;
        };
        let siblings: &[&str] = match key {
            "trait" => &["op", "value"],
            "event" => &["where", "agg", "window"],
            _ => &[],
        };
        for name in map.keys() {
            let known = name == key || siblings.contains(&name.as_str()) || (root && name == "v");
            if !known {
                self.report(&pointer_push(pointer, name), code::UNKNOWN_KEY);
            }
        }
        let at = pointer_push(pointer, key);
        let body = &map[key];
        match key {
            "all" | "any" => {
                let children = self.children(body, &at, depth)?;
                Some(if key == "all" {
                    Node::All(children)
                } else {
                    Node::Any(children)
                })
            }
            "not" => {
                let Some(inner) = body.as_object() else {
                    self.report(&at, code::MALFORMED);
                    return None;
                };
                let child = self.node(inner, &at, depth + 1, false)?;
                Some(Node::Not(Box::new(child)))
            }
            "trait" => {
                let name = self.string(body, &at);
                let condition = self.op_value(map, pointer);
                let (op, value) = condition?;
                Some(Node::Trait(TraitCondition {
                    key: name?,
                    op,
                    value,
                }))
            }
            "event" => self.event(map, body, pointer, &at),
            "list" => self.uuid(body, &at).map(Node::List),
            "segment" => self.uuid(body, &at).map(Node::Segment),
            "consent" => self.consent(body, &at),
            _ => self.string(body, &at).map(Node::Identifier),
        }
    }

    fn children(&mut self, body: &Value, pointer: &str, depth: usize) -> Option<Vec<Node>> {
        let Some(items) = body.as_array() else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        let mut children = Vec::with_capacity(items.len().min(MAX_NODES));
        let mut complete = true;
        for (index, item) in items.iter().enumerate() {
            if self.halted {
                return None;
            }
            let at = pointer_push(pointer, &index.to_string());
            let child = match item.as_object() {
                Some(inner) => self.node(inner, &at, depth + 1, false),
                None => {
                    self.report(&at, code::MALFORMED);
                    None
                }
            };
            match child {
                Some(child) => children.push(child),
                None => complete = false,
            }
        }
        complete.then_some(children)
    }

    fn uuid(&mut self, body: &Value, pointer: &str) -> Option<Uuid> {
        let id = body.as_str().and_then(|text| Uuid::parse_str(text).ok());
        if id.is_none() {
            self.report(pointer, code::MALFORMED);
        }
        id
    }

    fn consent(&mut self, body: &Value, pointer: &str) -> Option<Node> {
        let Some(map) = body.as_object() else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        self.unknown_keys(map, pointer, &["channel", "topic", "status"]);
        let channel = self.required_string(map, "channel", pointer);
        let status = self.required_string(map, "status", pointer);
        let mut topic_ok = true;
        let topic = match map.get("topic") {
            None => None,
            Some(value) => {
                let topic = self.string(value, &pointer_push(pointer, "topic"));
                topic_ok = topic.is_some();
                topic
            }
        };
        Some(Node::Consent(ConsentCondition {
            channel: channel?,
            topic: topic_ok.then_some(topic)?,
            status: status?,
        }))
    }

    fn event(
        &mut self,
        map: &Map<String, Value>,
        body: &Value,
        pointer: &str,
        at: &str,
    ) -> Option<Node> {
        let name = self.string(body, at);
        let mut complete = true;

        let mut filters = Vec::new();
        if let Some(value) = map.get("where") {
            let list_at = pointer_push(pointer, "where");
            match value.as_array() {
                None => {
                    self.report(&list_at, code::MALFORMED);
                    complete = false;
                }
                Some(items) => {
                    for (index, item) in items.iter().enumerate() {
                        let item_at = pointer_push(&list_at, &index.to_string());
                        if !self.count_node(&item_at) {
                            return None;
                        }
                        match self.filter(item, &item_at) {
                            Some(filter) => filters.push(filter),
                            None => complete = false,
                        }
                    }
                }
            }
        }

        let mut agg = None;
        if let Some(value) = map.get("agg") {
            agg = self.aggregate(value, &pointer_push(pointer, "agg"));
            complete &= agg.is_some();
        }

        let mut window = None;
        if let Some(value) = map.get("window") {
            window = self.window(value, &pointer_push(pointer, "window"));
            complete &= window.is_some();
        }

        complete.then_some(())?;
        Some(Node::Event(EventCondition {
            name: name?,
            filters,
            agg,
            window,
        }))
    }

    fn filter(&mut self, item: &Value, pointer: &str) -> Option<PropertyFilter> {
        let Some(map) = item.as_object() else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        self.unknown_keys(map, pointer, &["prop", "op", "value"]);
        let prop = self.required_string(map, "prop", pointer);
        let condition = self.op_value(map, pointer);
        let (op, value) = condition?;
        Some(PropertyFilter {
            prop: prop?,
            op,
            value,
        })
    }

    fn aggregate(&mut self, item: &Value, pointer: &str) -> Option<Aggregate> {
        let Some(map) = item.as_object() else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        self.unknown_keys(map, pointer, &["fn", "prop", "op", "value"]);
        let func = match map.get("fn") {
            Some(Value::String(name)) => {
                let func = AggFn::from_name(name);
                if func.is_none() {
                    self.report(&pointer_push(pointer, "fn"), code::AGGREGATE_NOT_ALLOWED);
                }
                func
            }
            _ => {
                self.report(&pointer_push(pointer, "fn"), code::MALFORMED);
                None
            }
        };
        let mut prop_ok = true;
        let prop = match map.get("prop") {
            None => None,
            Some(value) => {
                let prop = self.string(value, &pointer_push(pointer, "prop"));
                prop_ok = prop.is_some();
                prop
            }
        };
        let condition = self.op_value(map, pointer);
        let (op, value) = condition?;
        Some(Aggregate {
            func: func?,
            prop: prop_ok.then_some(prop)?,
            op,
            value,
        })
    }

    fn window(&mut self, item: &Value, pointer: &str) -> Option<Window> {
        let Some(map) = item.as_object() else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        self.unknown_keys(map, pointer, &["last", "between", "since_trait"]);
        let present: Vec<&str> = ["last", "between", "since_trait"]
            .into_iter()
            .filter(|key| map.contains_key(*key))
            .collect();
        let [key] = present[..] else {
            self.report(pointer, code::MALFORMED);
            return None;
        };
        let at = pointer_push(pointer, key);
        match key {
            "last" => self.string(&map[key], &at).map(Window::Last),
            "since_trait" => self.string(&map[key], &at).map(Window::SinceTrait),
            _ => match map[key].as_array().map(Vec::as_slice) {
                Some([Value::String(start), Value::String(end)]) => {
                    Some(Window::Between(start.clone(), end.clone()))
                }
                _ => {
                    self.report(&at, code::MALFORMED);
                    None
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn issues(value: Value) -> Vec<(String, &'static str)> {
        parse(&value)
            .expect_err("the definition should be refused")
            .into_iter()
            .map(|issue| (issue.pointer, issue.code))
            .collect()
    }

    fn pair(pointer: &str, code: &'static str) -> (String, &'static str) {
        (pointer.to_owned(), code)
    }

    #[test]
    fn every_node_kind_round_trips() {
        let id = "018f0000-0000-7000-8000-000000000001";
        let value = json!({
            "v": 1,
            "all": [
                {"any": [{"trait": "country", "op": "in", "value": ["DK", "SE"]}]},
                {"not": {"list": id}},
                {"segment": id},
                {"consent": {"channel": "email", "topic": "news", "status": "subscribed"}},
                {"identifier": "external:crm"},
                {"event": "order.placed",
                 "where": [{"prop": "total", "op": "gt", "value": 5}],
                 "agg": {"fn": "sum", "prop": "total", "op": "gte", "value": 100},
                 "window": {"last": "P90D"}},
                {"event": "visit", "window": {"between": ["2024-01-01T00:00:00Z", "2024-02-01T00:00:00Z"]}},
                {"event": "visit", "window": {"since_trait": "signed_up"}},
                {"trait": "vip", "op": "exists"}
            ]
        });
        let definition: Definition = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(definition.v, 1);
        assert_eq!(serde_json::to_value(&definition).unwrap(), value);
    }

    #[test]
    fn a_missing_or_non_numeric_version_is_malformed() {
        assert_eq!(issues(json!({"all": []})), [pair("/v", code::MALFORMED)]);
        assert_eq!(
            issues(json!({"v": "1", "all": []})),
            [pair("/v", code::MALFORMED)]
        );
        assert_eq!(issues(json!([])), [pair("", code::MALFORMED)]);
    }

    #[test]
    fn another_version_still_parses_so_the_validator_can_refuse_it() {
        assert_eq!(parse(&json!({"v": 2, "all": []})).unwrap().v, 2);
    }

    #[test]
    fn unknown_keys_are_refused_at_every_level() {
        assert_eq!(
            issues(json!({"v": 1, "all": [], "extra": 1})),
            [pair("/extra", code::UNKNOWN_KEY)]
        );
        assert_eq!(
            issues(
                json!({"v": 1, "all": [{"list": "018f0000-0000-7000-8000-000000000001", "op": "eq"}]})
            ),
            [pair("/all/0/op", code::UNKNOWN_KEY)]
        );
        assert_eq!(
            issues(
                json!({"v": 1, "consent": {"channel": "email", "status": "subscribed", "x": 1}})
            ),
            [pair("/consent/x", code::UNKNOWN_KEY)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "where": [{"prop": "p", "op": "exists", "z": 1}]})),
            [pair("/where/0/z", code::UNKNOWN_KEY)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "window": {"last": "P1D", "x": 1}})),
            [pair("/window/x", code::UNKNOWN_KEY),]
        );
    }

    #[test]
    fn an_object_with_two_node_keys_or_none_is_malformed() {
        assert_eq!(
            issues(json!({"v": 1, "all": [], "any": []})),
            [pair("", code::MALFORMED)]
        );
        assert_eq!(issues(json!({"v": 1})), [pair("", code::MALFORMED)]);
        assert_eq!(
            issues(json!({"v": 1, "all": [{}]})),
            [pair("/all/0", code::MALFORMED)]
        );
    }

    #[test]
    fn wrong_shapes_are_malformed() {
        assert_eq!(
            issues(json!({"v": 1, "all": {}})),
            [pair("/all", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "all": [1]})),
            [pair("/all/0", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "not": []})),
            [pair("/not", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "list": "nope"})),
            [pair("/list", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "trait": 5, "op": "exists"})),
            [pair("/trait", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "trait": "k"})),
            [pair("/op", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "window": {"between": ["2024-01-01T00:00:00Z"]}})),
            [pair("/window/between", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "window": {"last": "P1D", "since_trait": "t"}})),
            [pair("/window", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "consent": {"channel": "email"}})),
            [pair("/consent/status", code::MALFORMED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "where": {}})),
            [pair("/where", code::MALFORMED)]
        );
        assert_eq!(
            issues(
                json!({"v": 1, "event": "e", "agg": {"fn": "count", "prop": 3, "op": "gt", "value": 1}})
            ),
            [pair("/agg/prop", code::MALFORMED)]
        );
    }

    #[test]
    fn an_unknown_operator_or_function_gets_its_own_code() {
        assert_eq!(
            issues(json!({"v": 1, "trait": "k", "op": "like"})),
            [pair("/op", code::OPERATOR_NOT_ALLOWED)]
        );
        assert_eq!(
            issues(json!({"v": 1, "event": "e", "agg": {"fn": "median", "op": "gt", "value": 1}})),
            [pair("/agg/fn", code::AGGREGATE_NOT_ALLOWED)]
        );
    }

    #[test]
    fn issues_in_sibling_nodes_are_all_collected() {
        assert_eq!(
            issues(json!({"v": 1, "all": [{"list": 1}, {"segment": 2}]})),
            [
                pair("/all/0/list", code::MALFORMED),
                pair("/all/1/segment", code::MALFORMED)
            ]
        );
    }

    #[test]
    fn nesting_past_the_limit_is_refused_without_descending() {
        let mut value = json!({"all": []});
        for _ in 0..1_000 {
            value = json!({"not": value});
        }
        value["v"] = json!(1);
        let found = issues(value);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, code::TOO_DEEP);
        assert_eq!(found[0].0.matches("/not").count(), MAX_DEPTH);
    }

    #[test]
    fn exactly_the_depth_limit_is_accepted() {
        let mut value = json!({"all": []});
        for _ in 0..MAX_DEPTH - 1 {
            value = json!({"not": value});
        }
        value["v"] = json!(1);
        assert!(parse(&value).is_ok());
    }

    #[test]
    fn too_many_nodes_are_reported_once() {
        let children: Vec<Value> = (0..MAX_NODES).map(|_| json!({"all": []})).collect();
        let found = issues(json!({"v": 1, "all": children}));
        assert_eq!(found, [pair("/all/199", code::TOO_MANY_NODES)]);
    }

    #[test]
    fn where_filters_count_as_nodes() {
        let filters: Vec<Value> = (0..MAX_NODES)
            .map(|_| json!({"prop": "p", "op": "exists"}))
            .collect();
        let found = issues(json!({"v": 1, "event": "e", "where": filters}));
        assert_eq!(found, [pair("/where/199", code::TOO_MANY_NODES)]);
    }

    #[test]
    fn the_issue_list_is_capped() {
        let children: Vec<Value> = (0..MAX_NODES - 1).map(|_| json!({"list": 1})).collect();
        assert_eq!(
            issues(json!({"v": 1, "all": children})).len(),
            MAX_NODES - 1
        );
        let extras: Map<String, Value> = (0..MAX_ISSUES + 50)
            .map(|index| (format!("k{index}"), Value::Null))
            .collect();
        let mut value = Value::Object(extras);
        value["v"] = json!(1);
        value["all"] = json!([]);
        assert_eq!(issues(value).len(), MAX_ISSUES);
    }
}
