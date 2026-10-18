//! The publish checks that need the workspace: the catalog, the segments compiler, the egress
//! policy and the signing secret. They run after the structural validation of `core::flow` and
//! add to its issues; nothing a structural issue already names is reported twice.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use serde_json::{Value, json};
use time::OffsetDateTime;
use url::{Host, Url};
use uuid::Uuid;
use zirv_flow_core::egress::{Decision, EgressPolicy};
use zirv_flow_core::flow::{
    Audience, EventMatch, ExitCriterion, FlowGraph, FlowSettings, GRAPH_VERSION, Goal, Issue,
    MAX_CONDITIONS, MAX_NODES, MAX_TRAIT_KEYS, NodeKind, Trigger, code as core_code, validate,
};
use zirv_flow_core::segment::{self, code as segment_code};
use zirv_flow_segments::{Catalog, TraitStorage, compile};

/// The most exit criteria of one flow and the most issues returned, as `core::flow` has them: it
/// checks no more criteria than this and keeps no more issues, and so does this module.
const MAX_EXIT_CRITERIA: usize = 10;
const MAX_ISSUES: usize = 200;

/// The issue codes this module adds to the structural ones of `core::flow::code`.
pub(crate) mod code {
    pub(crate) const UNKNOWN_SEGMENT: &str = "unknown_segment";
    pub(crate) const UNKNOWN_LIST: &str = "unknown_list";
    pub(crate) const UNKNOWN_TRAIT: &str = "unknown_trait";
    pub(crate) const UNKNOWN_EVENT: &str = "unknown_event";
    pub(crate) const UNKNOWN_PROPERTY: &str = "unknown_property";
    pub(crate) const UNKNOWN_TOPIC: &str = "unknown_topic";
    pub(crate) const WEBHOOK_URL_REFUSED: &str = "webhook_url_refused";
    pub(crate) const WEBHOOK_SECRET_MISSING: &str = "webhook_secret_missing";

    /// Every code added here.
    pub(crate) const ALL: [&str; 8] = [
        UNKNOWN_SEGMENT,
        UNKNOWN_LIST,
        UNKNOWN_TRAIT,
        UNKNOWN_EVENT,
        UNKNOWN_PROPERTY,
        UNKNOWN_TOPIC,
        WEBHOOK_URL_REFUSED,
        WEBHOOK_SECRET_MISSING,
    ];
}

/// What the checks read from the workspace, all taken in one transaction.
pub(crate) struct Environment<'a> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) storage: &'a TraitStorage,
    /// The database time conditions are compiled at.
    pub(crate) now: OffsetDateTime,
    pub(crate) egress: &'a EgressPolicy,
    /// Whether the workspace has a webhook signing secret.
    pub(crate) signing_secret: bool,
}

/// The issues of a flow and what it reads, for the impact preview.
pub(crate) struct Checked {
    pub(crate) issues: Vec<Issue>,
    /// The merged dependency summary of every condition, event match and reference in the flow,
    /// in the shape of a segment version's `deps`.
    pub(crate) reads: Value,
}

/// Validates the flow: the structure first, then the catalog and the rest.
pub(crate) fn check(
    graph: &FlowGraph,
    settings: &FlowSettings,
    environment: &Environment<'_>,
) -> Checked {
    let mut checker = Checker {
        environment,
        issues: validate(graph, settings),
        reads: Reads::default(),
    };
    checker.nodes(graph);
    checker.settings(settings);
    checker.issues.truncate(MAX_ISSUES);
    Checked {
        issues: checker.issues,
        reads: checker.reads.to_value(),
    }
}

#[derive(Default)]
struct Reads {
    keys: BTreeMap<&'static str, BTreeSet<String>>,
}

impl Reads {
    fn add(&mut self, key: &'static str, value: String) {
        self.keys.entry(key).or_default().insert(value);
    }

    fn absorb(&mut self, deps: &Value) {
        for key in ["traits", "events", "lists", "segments"] {
            for item in deps
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                self.add(key, item.to_owned());
            }
        }
    }

    fn to_value(&self) -> Value {
        let list = |key: &str| -> Vec<&String> {
            self.keys.get(key).into_iter().flatten().collect::<Vec<_>>()
        };
        json!({
            "traits": list("traits"),
            "events": list("events"),
            "lists": list("lists"),
            "segments": list("segments"),
        })
    }
}

struct Checker<'a> {
    environment: &'a Environment<'a>,
    issues: Vec<Issue>,
    reads: Reads,
}

/// The flow code for a segment issue that names something the workspace does not have.
fn reference_code(segment_issue: &str) -> Option<&'static str> {
    match segment_issue {
        segment_code::UNKNOWN_TRAIT => Some(code::UNKNOWN_TRAIT),
        segment_code::UNKNOWN_EVENT => Some(code::UNKNOWN_EVENT),
        segment_code::UNKNOWN_PROPERTY => Some(code::UNKNOWN_PROPERTY),
        segment_code::UNKNOWN_LIST => Some(code::UNKNOWN_LIST),
        segment_code::UNKNOWN_SEGMENT => Some(code::UNKNOWN_SEGMENT),
        segment_code::UNKNOWN_TOPIC => Some(code::UNKNOWN_TOPIC),
        _ => None,
    }
}

/// The segment parser calls the filters `where`; on the flow wire the field is `filters`.
fn filters_pointer(pointer: &str) -> String {
    match pointer.strip_prefix("/where") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("/filters{rest}"),
        _ => pointer.to_owned(),
    }
}

impl Checker<'_> {
    fn issue(&mut self, code: &'static str, node: Option<&str>, at: Option<String>) {
        let issue = match node {
            Some(node) => Issue::node(code, node),
            None => Issue::general(code),
        };
        self.issues.push(match at {
            Some(at) => issue.at(at),
            None => issue,
        });
    }

    fn nodes(&mut self, graph: &FlowGraph) {
        // The structural validation refuses a graph over its limits before it reads the nodes;
        // so does this.
        if graph.v != GRAPH_VERSION || graph.nodes.len() > MAX_NODES {
            return;
        }
        for node in &graph.nodes {
            let id = Some(node.id.as_str());
            match &node.kind {
                NodeKind::WaitUntilEvent { event, .. } => self.event(event, id, ""),
                NodeKind::Branch { conditions } => {
                    for (index, condition) in conditions.iter().enumerate().take(MAX_CONDITIONS) {
                        self.condition(condition, id, &format!("/conditions/{index}"));
                    }
                }
                NodeKind::ActionWebhook { url, trait_keys } => {
                    self.webhook(url, trait_keys, &node.id);
                }
                NodeKind::ActionUpdateTrait { key, .. } => {
                    if !self.environment.catalog.traits.contains_key(key) {
                        self.issue(code::UNKNOWN_TRAIT, id, Some("/key".to_owned()));
                    }
                }
                NodeKind::ActionList { list_id, .. } => {
                    self.list(*list_id, id, "/list_id");
                }
                NodeKind::WaitDuration { .. }
                | NodeKind::WaitUntilTime { .. }
                | NodeKind::SplitRandom { .. }
                | NodeKind::Goal { .. }
                | NodeKind::Exit {} => {}
            }
        }
    }

    fn settings(&mut self, settings: &FlowSettings) {
        match &settings.trigger {
            Trigger::Segment { segment_id, .. } => {
                self.segment(*segment_id, "/trigger/segment_id");
            }
            Trigger::Event(event) => self.event(event, None, "/trigger"),
            Trigger::Schedule { audience, .. } => {
                if let Audience::Segment { segment_id } = audience {
                    self.segment(*segment_id, "/trigger/audience/segment_id");
                }
            }
            Trigger::Manual {} | Trigger::Api {} => {}
        }
        match &settings.goal {
            Some(Goal::Event(event)) => self.event(event, None, "/goal"),
            Some(Goal::Segment { segment_id }) => self.segment(*segment_id, "/goal/segment_id"),
            None => {}
        }
        for (index, criterion) in settings
            .exit_criteria
            .iter()
            .enumerate()
            .take(MAX_EXIT_CRITERIA)
        {
            if let ExitCriterion::Event(event) = criterion {
                self.event(event, None, &format!("/exit_criteria/{index}"));
            }
        }
    }

    fn segment(&mut self, id: Uuid, at: &str) {
        if self.environment.catalog.segments.contains(&id) {
            self.reads.add("segments", id.to_string());
        } else {
            self.issue(code::UNKNOWN_SEGMENT, None, Some(at.to_owned()));
        }
    }

    fn list(&mut self, id: Uuid, node: Option<&str>, at: &str) {
        if self.environment.catalog.lists.contains(&id) {
            self.reads.add("lists", id.to_string());
        } else {
            self.issue(code::UNKNOWN_LIST, node, Some(at.to_owned()));
        }
    }

    /// An event name with property filters, compiled as the segment `{"event": ..., "where": ...}`.
    /// One the structural validation refused is not compiled: it is already reported.
    fn event(&mut self, event: &EventMatch, node: Option<&str>, base: &str) {
        let mut wire = json!({"v": 1, "event": event.name});
        if !event.filters.is_empty() {
            wire["where"] = Value::Array(event.filters.clone());
        }
        let Some(problems) = self.compile(&wire) else {
            return;
        };
        for problem in problems {
            let pointer = filters_pointer(&problem.pointer);
            let code = reference_code(problem.code).unwrap_or(core_code::INVALID_FILTER);
            self.issue(code, node, Some(format!("{base}{pointer}")));
        }
    }

    /// A branch condition, a segment AST node in its wire form.
    fn condition(&mut self, condition: &Value, node: Option<&str>, base: &str) {
        let Some(map) = condition.as_object() else {
            return;
        };
        if map.contains_key("v") {
            return;
        }
        let mut wire = map.clone();
        wire.insert("v".to_owned(), 1.into());
        let Some(problems) = self.compile(&Value::Object(wire)) else {
            return;
        };
        for problem in problems {
            let code = reference_code(problem.code).unwrap_or(core_code::INVALID_CONDITION);
            self.issue(code, node, Some(format!("{base}{}", problem.pointer)));
        }
    }

    /// Compiles a segment definition against the catalog and records what it reads. Returns the
    /// issues of a definition that parses but does not compile; `None` when it compiled, and when
    /// it does not parse, which the structural validation has already reported.
    fn compile(&mut self, wire: &Value) -> Option<Vec<segment::Issue>> {
        let definition = segment::parse(wire).ok()?;
        match compile(
            &definition,
            self.environment.catalog,
            self.environment.storage,
            self.environment.now,
        ) {
            Ok(compiled) => {
                self.reads.absorb(&compiled.deps.to_value());
                None
            }
            Err(problems) => Some(problems),
        }
    }

    fn webhook(&mut self, url: &str, trait_keys: &[String], node: &str) {
        let id = Some(node);
        if let Some(refused) = self.refused(url) {
            self.issue(refused, id, Some("/url".to_owned()));
        }
        for (index, key) in trait_keys.iter().enumerate().take(MAX_TRAIT_KEYS) {
            if self.environment.catalog.traits.contains_key(key) {
                self.reads.add("traits", key.clone());
            } else {
                self.issue(
                    code::UNKNOWN_TRAIT,
                    id,
                    Some(format!("/trait_keys/{index}")),
                );
            }
        }
        if !self.environment.signing_secret {
            self.issue(code::WEBHOOK_SECRET_MISSING, id, None);
        }
    }

    /// The code when the egress policy refuses the URL's literal address. A host name is decided
    /// when the call connects, after it resolves, as the guarded HTTP client does; publish has no
    /// DNS to ask. A URL the structural validation refused is left to it.
    fn refused(&self, url: &str) -> Option<&'static str> {
        let host = Url::parse(url).ok()?;
        let ip = match host.host()? {
            Host::Ipv4(ip) => IpAddr::V4(ip),
            Host::Ipv6(ip) => IpAddr::V6(ip),
            Host::Domain(_) => return None,
        };
        matches!(self.environment.egress.decide(ip), Decision::Deny(_))
            .then_some(code::WEBHOOK_URL_REFUSED)
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::HashMap;
    use serde_json::json;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;

    use super::*;

    const SEGMENT: Uuid = Uuid::from_u128(1);
    const LIST: Uuid = Uuid::from_u128(2);

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("age".into(), TraitType::Number);
        catalog.events.insert(
            "signed_up".into(),
            HashMap::from([("plan".to_owned(), PropertyType::String)]),
        );
        catalog.lists.insert(LIST);
        catalog.segments.insert(SEGMENT);
        catalog
    }

    struct Setup {
        catalog: Catalog,
        egress: EgressPolicy,
        signing_secret: bool,
    }

    impl Default for Setup {
        fn default() -> Self {
            Self {
                catalog: catalog(),
                egress: EgressPolicy::default(),
                signing_secret: true,
            }
        }
    }

    impl Setup {
        fn check(&self, graph: Value, settings: Value) -> Checked {
            let storage = TraitStorage::default();
            let environment = Environment {
                catalog: &self.catalog,
                storage: &storage,
                now: OffsetDateTime::UNIX_EPOCH,
                egress: &self.egress,
                signing_secret: self.signing_secret,
            };
            check(
                &serde_json::from_value(graph).unwrap(),
                &serde_json::from_value(settings).unwrap(),
                &environment,
            )
        }
    }

    fn manual() -> Value {
        json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
    }

    /// A graph with one node `n` in front of an exit: `n`'s ports all lead to `end`.
    fn around(node: Value, ports: &[&str]) -> Value {
        let edges: Vec<Value> = ports
            .iter()
            .enumerate()
            .map(|(index, port)| json!({"id": format!("e{index}"), "from": "n", "port": port, "to": "end"}))
            .collect();
        json!({"v": 1, "nodes": [node, {"id": "end", "type": "exit"}], "edges": edges})
    }

    fn codes(checked: &Checked) -> Vec<String> {
        checked
            .issues
            .iter()
            .map(|issue| {
                format!(
                    "{} {} {}",
                    issue.code,
                    issue.node.as_deref().unwrap_or("-"),
                    issue.at.as_deref().unwrap_or("-")
                )
            })
            .collect()
    }

    #[test]
    fn a_valid_flow_has_no_issues_and_its_reads_are_collected() {
        let graph = json!({"v": 1, "nodes": [
            {"id": "n", "type": "branch", "conditions": [
                {"trait": "plan", "op": "eq", "value": "pro"}, {"segment": SEGMENT}]},
            {"id": "list", "type": "action.list", "list_id": LIST, "change": "add"},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e0", "from": "n", "port": "case_0", "to": "list"},
            {"id": "e1", "from": "n", "port": "case_1", "to": "list"},
            {"id": "e2", "from": "n", "port": "else", "to": "end"},
            {"id": "e3", "from": "list", "port": "next", "to": "end"}
        ]});
        let settings = json!({
            "trigger": {"type": "event", "event": "signed_up",
                "filters": [{"prop": "plan", "op": "eq", "value": "pro"}]},
            "reentry": {"mode": "once"}});

        let checked = Setup::default().check(graph, settings);

        assert_eq!(codes(&checked), Vec::<String>::new());
        assert_eq!(checked.reads["traits"], json!(["plan"]));
        assert_eq!(checked.reads["events"], json!(["signed_up"]));
        assert_eq!(checked.reads["lists"], json!([LIST.to_string()]));
        assert_eq!(checked.reads["segments"], json!([SEGMENT.to_string()]));
    }

    #[test]
    fn references_to_what_the_workspace_lacks_are_issues_with_the_node_and_the_field() {
        let missing = Uuid::from_u128(9).to_string();
        let graph = json!({"v": 1, "nodes": [
            {"id": "t", "type": "action.update_trait", "key": "ghost",
                "change": {"op": "set", "value": 1}},
            {"id": "l", "type": "action.list", "list_id": missing, "change": "remove"},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e0", "from": "t", "port": "next", "to": "l"},
            {"id": "e1", "from": "l", "port": "next", "to": "end"}
        ]});
        let settings = json!({
            "trigger": {"type": "segment", "segment_id": missing, "on": "entered"},
            "reentry": {"mode": "once"},
            "goal": {"type": "segment", "segment_id": missing},
            "exit_criteria": [{"type": "event", "event": "ghost_event"}]
        });

        let checked = Setup::default().check(graph, settings);

        assert_eq!(
            codes(&checked),
            [
                "unknown_trait t /key",
                "unknown_list l /list_id",
                "unknown_segment - /trigger/segment_id",
                "unknown_segment - /goal/segment_id",
                "unknown_event - /exit_criteria/0/event",
            ]
        );
    }

    #[test]
    fn a_condition_that_does_not_compile_is_reported_at_its_own_pointer() {
        let graph = around(
            json!({"id": "n", "type": "branch", "conditions": [
                {"trait": "plan", "op": "eq", "value": "pro"},
                {"trait": "ghost", "op": "eq", "value": "x"},
                {"trait": "age", "op": "eq", "value": "not a number"},
                {"list": Uuid::from_u128(9)}
            ]}),
            &["case_0", "case_1", "case_2", "case_3", "else"],
        );

        let checked = Setup::default().check(graph, manual());

        assert_eq!(
            codes(&checked),
            [
                "unknown_trait n /conditions/1/trait",
                "invalid_condition n /conditions/2/value",
                "unknown_list n /conditions/3/list",
            ]
        );
    }

    #[test]
    fn an_event_filter_is_checked_against_the_events_schema() {
        let setup = Setup::default();
        let settings = |filters: Value| {
            json!({"trigger": {"type": "event", "event": "signed_up", "filters": filters},
                "reentry": {"mode": "once"}})
        };
        let graph = || json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []});

        let unknown_property = setup.check(
            graph(),
            settings(json!([{"prop": "ghost", "op": "eq", "value": "x"}])),
        );
        let wrong_type = setup.check(
            graph(),
            settings(json!([{"prop": "plan", "op": "gt", "value": 3}])),
        );

        assert_eq!(
            codes(&unknown_property),
            ["unknown_property - /trigger/filters/0/prop"]
        );
        assert_eq!(codes(&wrong_type).len(), 1);
        assert!(codes(&wrong_type)[0].starts_with("invalid_filter - /trigger/filters/0"));
    }

    #[test]
    fn what_the_structure_check_already_reported_is_not_reported_again() {
        let graph = around(
            json!({"id": "n", "type": "branch", "conditions": [{"trait": "plan", "op": "nope"}]}),
            &["case_0", "else"],
        );

        let checked = Setup::default().check(graph, manual());

        let invalid: Vec<_> = checked
            .issues
            .iter()
            .filter(|issue| issue.code == core_code::INVALID_CONDITION)
            .collect();
        assert_eq!(invalid.len(), 1);
        assert_eq!(checked.issues.len(), 1);
    }

    #[test]
    fn a_webhook_to_a_private_literal_address_is_refused_unless_the_policy_allows_it() {
        let node = |url: &str| {
            around(
                json!({"id": "n", "type": "action.webhook", "url": url}),
                &["next"],
            )
        };
        let setup = Setup::default();
        let allowing = Setup {
            egress: EgressPolicy::parse(&["10.0.0.0/8".to_owned()]).unwrap(),
            ..Setup::default()
        };

        let private = setup.check(node("https://10.1.2.3/hook"), manual());
        let metadata = setup.check(node("http://169.254.169.254/latest"), manual());
        let loopback = setup.check(node("https://[::1]:8443/hook"), manual());
        let public = setup.check(node("https://8.8.8.8/hook"), manual());
        let by_name = setup.check(node("https://hooks.example.com/hook"), manual());
        let allowed = allowing.check(node("https://10.1.2.3/hook"), manual());
        let still_loopback = allowing.check(node("https://127.0.0.1/hook"), manual());

        for refused in [&private, &metadata, &loopback, &still_loopback] {
            assert_eq!(codes(refused), ["webhook_url_refused n /url"]);
        }
        for passed in [&public, &by_name, &allowed] {
            assert_eq!(codes(passed), Vec::<String>::new());
        }
    }

    #[test]
    fn a_webhook_needs_the_workspace_signing_secret_and_known_trait_keys() {
        let graph = around(
            json!({"id": "n", "type": "action.webhook", "url": "https://hooks.example.com/h",
                "trait_keys": ["plan", "ghost"]}),
            &["next"],
        );
        let without = Setup {
            signing_secret: false,
            ..Setup::default()
        };

        let missing = without.check(graph.clone(), manual());
        let present = Setup::default().check(graph, manual());

        assert_eq!(
            codes(&missing),
            [
                "unknown_trait n /trait_keys/1",
                "webhook_secret_missing n -"
            ]
        );
        assert_eq!(codes(&present), ["unknown_trait n /trait_keys/1"]);
        assert_eq!(present.reads["traits"], json!(["plan"]));
    }

    #[test]
    fn a_graph_without_a_webhook_needs_no_secret() {
        let without = Setup {
            signing_secret: false,
            ..Setup::default()
        };
        let graph = json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []});

        assert!(without.check(graph, manual()).issues.is_empty());
    }

    #[test]
    fn the_local_limits_are_the_structural_validators() {
        let criteria = |count: usize| {
            let one = json!({"type": "event", "event": "signed_up"});
            json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"},
                "exit_criteria": vec![one; count]})
        };
        let graph = || json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []});
        let setup = Setup::default();

        let at_the_limit = setup.check(graph(), criteria(MAX_EXIT_CRITERIA));
        let over = setup.check(graph(), criteria(MAX_EXIT_CRITERIA + 1));

        assert!(at_the_limit.issues.is_empty());
        assert_eq!(codes(&over), ["too_many_exit_criteria - /exit_criteria"]);
        let many_unknown = json!({"v": 1, "nodes": [], "edges": []});
        let flood = |count: usize| {
            let ghost = json!({"type": "event", "event": "ghost"});
            json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"},
                "exit_criteria": vec![ghost; count]})
        };
        let checked = setup.check(many_unknown, flood(MAX_EXIT_CRITERIA));
        assert_eq!(checked.issues.len(), MAX_EXIT_CRITERIA + 1);
        assert!(checked.issues.len() <= MAX_ISSUES);
    }

    #[test]
    fn the_added_codes_are_unique_snake_case_and_apart_from_the_structural_ones() {
        let structural: BTreeSet<&str> = core_code::ALL.into_iter().collect();
        let mut seen = BTreeSet::new();
        for text in code::ALL {
            assert!(seen.insert(text), "{text} is listed twice");
            assert!(
                text.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            );
            assert!(!structural.contains(text), "{text} is a structural code");
        }
    }
}
