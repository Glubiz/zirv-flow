//! Structural publish validation (architecture section 9.1). It returns every issue at once and
//! needs no catalog or database: that segment and list ids exist, and that conditions compile
//! against the catalog, are checked by the publish API. Conditions are checked here for their
//! shape, by the segment parser, which is all `core` can reach.

use chrono::{NaiveDateTime, NaiveTime};
use hashbrown::{HashMap, HashSet};
use serde_json::Value;
use url::Url;

use super::graph::{
    Edge, EventMatch, FlowGraph, FlowNode, GRAPH_VERSION, NodeKind, TraitChange, UntilTime,
};
use super::issue::{Issue, code};
use super::settings::{ExitCriterion, FlowSettings, Goal, Reentry, Trigger};
use crate::schedule;
use crate::segment;

/// The most nodes in one graph.
pub const MAX_NODES: usize = 200;
/// The most conditions in one branch (it has one more port, `else`).
pub const MAX_CONDITIONS: usize = 10;
/// The fewest and the most arms of one split.
pub const MIN_ARMS: usize = 2;
pub const MAX_ARMS: usize = 10;
/// The longest wait, timeout or re-entry interval: 365 days.
pub const MAX_DURATION_SECONDS: u64 = 365 * 24 * 60 * 60;
/// The longest node or edge id.
pub const MAX_ID_CHARS: usize = 64;
/// The most trait keys a webhook may send.
pub const MAX_TRAIT_KEYS: usize = 20;
/// The most exit criteria of one flow.
pub const MAX_EXIT_CRITERIA: usize = 10;
const MAX_KEY_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 200;
const MAX_URL_CHARS: usize = 2048;
/// The most edges in one graph: every port of the most ports 200 nodes can have.
pub const MAX_EDGES: usize = MAX_NODES * (MAX_CONDITIONS + 1);
/// The most issues returned, as in the segment parser; the rest are dropped.
pub const MAX_ISSUES: usize = 200;
/// The longest string in a trait value set by a flow.
const MAX_VALUE_STRING_CHARS: usize = 1000;

/// Validates the graph and the settings together, collecting every issue.
pub fn validate(graph: &FlowGraph, settings: &FlowSettings) -> Vec<Issue> {
    let mut issues = validate_graph(graph);
    issues.extend(validate_settings(settings));
    issues.truncate(MAX_ISSUES);
    issues
}

/// Validates the graph alone.
pub fn validate_graph(graph: &FlowGraph) -> Vec<Issue> {
    let mut issues = Vec::new();
    if graph.v != GRAPH_VERSION {
        return vec![Issue::general(code::UNSUPPORTED_VERSION)];
    }
    // The limits come first: nothing below runs on a graph that exceeds them.
    if graph.nodes.len() > MAX_NODES {
        issues.push(Issue::general(code::TOO_MANY_NODES));
    }
    if graph.edges.len() > MAX_EDGES {
        issues.push(Issue::general(code::TOO_MANY_EDGES));
    }
    if !issues.is_empty() {
        return issues;
    }

    // The first node of an id owns it; a later one is a duplicate.
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (index, node) in graph.nodes.iter().enumerate() {
        if !valid_id(&node.id) {
            issues.push(Issue::general(code::INVALID_ID).at(format!("/nodes/{index}/id")));
        }
        if index_of.contains_key(node.id.as_str()) {
            issues.push(Issue::node(code::DUPLICATE_NODE_ID, &node.id));
        } else {
            index_of.insert(&node.id, index);
        }
        check_node(node, &mut issues);
    }

    let resolved = check_edges(graph, &index_of, &mut issues);
    check_shape(graph, &index_of, &resolved, &mut issues);
    issues.truncate(MAX_ISSUES);
    issues
}

/// Validates the settings alone.
pub fn validate_settings(settings: &FlowSettings) -> Vec<Issue> {
    let mut issues = Vec::new();
    match &settings.trigger {
        Trigger::Event(event) => check_event(event, None, "/trigger", &mut issues),
        Trigger::Schedule { cron, timezone, .. } => {
            if schedule::validate(cron, "UTC").is_err() {
                issues.push(Issue::general(code::INVALID_CRON).at("/trigger/cron"));
            }
            if schedule::validate("* * * * *", timezone).is_err() {
                issues.push(Issue::general(code::INVALID_TIMEZONE).at("/trigger/timezone"));
            }
        }
        Trigger::Segment { .. } | Trigger::Manual {} | Trigger::Api {} => {}
    }
    if let Reentry::Always {
        min_interval_seconds,
    } = settings.reentry
        && min_interval_seconds > MAX_DURATION_SECONDS
    {
        issues.push(Issue::general(code::INVALID_DURATION).at("/reentry/min_interval_seconds"));
    }
    if let Some(Goal::Event(event)) = &settings.goal {
        check_event(event, None, "/goal", &mut issues);
    }
    if settings.exit_on_goal && settings.goal.is_none() {
        issues.push(Issue::general(code::EXIT_ON_GOAL_WITHOUT_GOAL).at("/exit_on_goal"));
    }
    if settings.exit_criteria.len() > MAX_EXIT_CRITERIA {
        issues.push(Issue::general(code::TOO_MANY_EXIT_CRITERIA).at("/exit_criteria"));
    }
    for (index, criterion) in settings
        .exit_criteria
        .iter()
        .enumerate()
        .take(MAX_EXIT_CRITERIA)
    {
        let at = format!("/exit_criteria/{index}");
        match criterion {
            ExitCriterion::LeavesSegment {} => {
                if !matches!(settings.trigger, Trigger::Segment { .. }) {
                    issues
                        .push(Issue::general(code::LEAVES_SEGMENT_WITHOUT_SEGMENT_TRIGGER).at(at));
                }
            }
            ExitCriterion::Event(event) => check_event(event, None, &at, &mut issues),
            ExitCriterion::GoalReached {} => {
                if settings.goal.is_none() {
                    issues.push(Issue::general(code::GOAL_CRITERION_WITHOUT_GOAL).at(at));
                }
            }
        }
    }
    issues.truncate(MAX_ISSUES);
    issues
}

fn valid_id(id: &str) -> bool {
    (1..=MAX_ID_CHARS).contains(&id.chars().count())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_duration(seconds: u64) -> bool {
    (1..=MAX_DURATION_SECONDS).contains(&seconds)
}

fn issue(node: Option<&str>, code: &'static str) -> Issue {
    node.map_or_else(|| Issue::general(code), |id| Issue::node(code, id))
}

/// Checks an event name and its filters; `base` is the pointer of the object that holds them.
fn check_event(event: &EventMatch, node: Option<&str>, base: &str, out: &mut Vec<Issue>) {
    let name = event.name.trim();
    if name.is_empty() || event.name.chars().count() > MAX_NAME_CHARS {
        out.push(issue(node, code::INVALID_EVENT_NAME).at(format!("{base}/event")));
        return;
    }
    if let Err(problems) = event.to_condition() {
        for problem in problems {
            let pointer = filters_pointer(&problem.pointer);
            out.push(issue(node, code::INVALID_FILTER).at(format!("{base}{pointer}")));
        }
    }
}

/// The segment parser calls the filters `where`; on the flow wire the field is `filters`.
fn filters_pointer(pointer: &str) -> String {
    match pointer.strip_prefix("/where") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("/filters{rest}"),
        _ => pointer.to_owned(),
    }
}

fn check_node(node: &FlowNode, out: &mut Vec<Issue>) {
    let id = Some(node.id.as_str());
    match &node.kind {
        NodeKind::WaitDuration { seconds } => {
            if !valid_duration(*seconds) {
                out.push(issue(id, code::INVALID_DURATION).at("/seconds"));
            }
        }
        NodeKind::WaitUntilTime { when } => check_until_time(when, &node.id, out),
        NodeKind::WaitUntilEvent {
            event,
            timeout_seconds,
        } => {
            check_event(event, id, "", out);
            if !valid_duration(*timeout_seconds) {
                out.push(issue(id, code::INVALID_DURATION).at("/timeout_seconds"));
            }
        }
        NodeKind::Branch { conditions } => check_branch(conditions, &node.id, out),
        NodeKind::SplitRandom { arms } => {
            if arms.len() < MIN_ARMS {
                out.push(issue(id, code::TOO_FEW_ARMS).at("/arms"));
            }
            if arms.len() > MAX_ARMS {
                out.push(issue(id, code::TOO_MANY_ARMS).at("/arms"));
            }
            for (index, arm) in arms.iter().enumerate().take(MAX_ARMS) {
                if arm.weight == 0 {
                    out.push(issue(id, code::INVALID_WEIGHT).at(format!("/arms/{index}/weight")));
                }
            }
            if arms.len() <= MAX_ARMS
                && arms.iter().map(|arm| u64::from(arm.weight)).sum::<u64>() != 100
            {
                out.push(issue(id, code::WEIGHTS_NOT_100).at("/arms"));
            }
        }
        NodeKind::ActionWebhook {
            url, trait_keys, ..
        } => {
            if !valid_url(url) {
                out.push(issue(id, code::INVALID_URL).at("/url"));
            }
            if trait_keys.len() > MAX_TRAIT_KEYS {
                out.push(issue(id, code::TOO_MANY_TRAIT_KEYS).at("/trait_keys"));
            }
            let mut seen = HashSet::new();
            for (index, key) in trait_keys.iter().enumerate().take(MAX_TRAIT_KEYS) {
                if !valid_key(key) || !seen.insert(key.as_str()) {
                    out.push(issue(id, code::INVALID_TRAIT_KEY).at(format!("/trait_keys/{index}")));
                }
            }
        }
        NodeKind::ActionUpdateTrait { key, change } => {
            if !valid_key(key) {
                out.push(issue(id, code::INVALID_TRAIT_KEY).at("/key"));
            }
            match change {
                TraitChange::Set { value } if !valid_value(value) => {
                    out.push(issue(id, code::INVALID_TRAIT_VALUE).at("/change/value"));
                }
                TraitChange::Increment { by } if !by.is_finite() => {
                    out.push(issue(id, code::INVALID_TRAIT_VALUE).at("/change/by"));
                }
                _ => {}
            }
        }
        NodeKind::ActionList { .. } | NodeKind::Goal { .. } | NodeKind::Exit {} => {}
    }
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.chars().count() <= MAX_KEY_CHARS
}

/// A trait value a flow sets: within the segment AST's depth and node limits, with bounded strings
/// and keys. Walks iteratively and stops at the first breach, so the work is bounded too.
fn valid_value(value: &Value) -> bool {
    let mut nodes = 0;
    let mut pending = vec![(value, 1)];
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > segment::MAX_NODES || depth > segment::MAX_DEPTH {
            return false;
        }
        match value {
            Value::String(text) => {
                if text.chars().take(MAX_VALUE_STRING_CHARS + 1).count() > MAX_VALUE_STRING_CHARS {
                    return false;
                }
            }
            Value::Array(items) => {
                if items.len() > segment::MAX_NODES {
                    return false;
                }
                pending.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Object(map) => {
                if map.len() > segment::MAX_NODES
                    || map.keys().any(|key| key.chars().count() > MAX_KEY_CHARS)
                {
                    return false;
                }
                pending.extend(map.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    true
}

/// An absolute `http` or `https` URL with a host. The egress policy decides what it may reach.
fn valid_url(url: &str) -> bool {
    let rest = ["https://", "http://"].iter().find_map(|scheme| {
        let head = url.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme)
            .then(|| &url[scheme.len()..])
    });
    url.chars().count() <= MAX_URL_CHARS
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && rest.is_some_and(|rest| !rest.is_empty() && !rest.starts_with(['/', '?', '#', ':']))
        && Url::parse(url).is_ok_and(|parsed| {
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some_and(|host| !host.is_empty())
        })
}

/// Whether `text` has exactly the shape of `pattern`, where `9` stands for one ASCII digit and any
/// other character for itself.
fn shaped(text: &str, pattern: &str) -> bool {
    text.len() == pattern.len()
        && text.bytes().zip(pattern.bytes()).all(|(got, want)| {
            if want == b'9' {
                got.is_ascii_digit()
            } else {
                got == want
            }
        })
}

fn check_until_time(when: &UntilTime, node: &str, out: &mut Vec<Issue>) {
    let id = Some(node);
    match when {
        UntilTime::TimeOfDay { time, weekdays } => {
            if !shaped(time, "99:99") || NaiveTime::parse_from_str(time, "%H:%M").is_err() {
                out.push(issue(id, code::INVALID_TIME).at("/when/time"));
            }
            let unique: HashSet<_> = weekdays.iter().collect();
            if unique.len() != weekdays.len() {
                out.push(issue(id, code::INVALID_WEEKDAYS).at("/when/weekdays"));
            }
        }
        UntilTime::DateTime { at } => {
            // chrono reads second 60 as a leap second; a flow wait has none.
            if !shaped(at, "9999-99-99T99:99:99")
                || at.ends_with(":60")
                || NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S").is_err()
            {
                out.push(issue(id, code::INVALID_TIME).at("/when/at"));
            }
        }
    }
}

fn check_branch(conditions: &[Value], node: &str, out: &mut Vec<Issue>) {
    let id = Some(node);
    if conditions.is_empty() {
        out.push(issue(id, code::NO_CONDITIONS).at("/conditions"));
    }
    if conditions.len() > MAX_CONDITIONS {
        out.push(issue(id, code::TOO_MANY_CONDITIONS).at("/conditions"));
    }
    for (index, condition) in conditions.iter().enumerate().take(MAX_CONDITIONS) {
        let base = format!("/conditions/{index}");
        let Some(map) = condition.as_object() else {
            out.push(issue(id, code::INVALID_CONDITION).at(base));
            continue;
        };
        if map.contains_key("v") {
            out.push(issue(id, code::INVALID_CONDITION).at(format!("{base}/v")));
            continue;
        }
        let mut wire = map.clone();
        wire.insert("v".to_owned(), 1.into());
        if let Err(problems) = segment::parse(&Value::Object(wire)) {
            for problem in problems {
                out.push(
                    issue(id, code::INVALID_CONDITION).at(format!("{base}{}", problem.pointer)),
                );
            }
        }
    }
}

/// Checks ids, endpoints and ports of the edges. Returns the edges that join two known nodes, as
/// node indexes, whatever their port: the shape checks need them even when the port is wrong.
fn check_edges(
    graph: &FlowGraph,
    index_of: &HashMap<&str, usize>,
    out: &mut Vec<Issue>,
) -> Vec<(usize, usize)> {
    let mut edge_ids: HashSet<&str> = HashSet::new();
    let mut wired: HashSet<(usize, &str)> = HashSet::new();
    let mut joined = Vec::new();
    for (position, edge) in graph.edges.iter().enumerate() {
        check_edge(edge, index_of, graph, &mut wired, out);
        if !valid_id(&edge.id) {
            out.push(Issue::general(code::INVALID_ID).at(format!("/edges/{position}/id")));
        }
        if !edge_ids.insert(&edge.id) {
            out.push(Issue::edge(code::DUPLICATE_EDGE_ID, &edge.id));
        }
        if let (Some(&from), Some(&to)) = (
            index_of.get(edge.from.as_str()),
            index_of.get(edge.to.as_str()),
        ) {
            joined.push((from, to));
        }
    }
    for (index, node) in graph.nodes.iter().enumerate() {
        // An oversized node is refused for its size; its ports are not listed.
        if index_of.get(node.id.as_str()) != Some(&index) || oversized(&node.kind) {
            continue;
        }
        for port in node.kind.ports() {
            if !wired.contains(&(index, port.as_str())) {
                out.push(Issue::node(code::PORT_NOT_WIRED, &node.id).at(format!("/ports/{port}")));
            }
        }
    }
    joined
}

/// A branch or split with more ports than the limits allow.
fn oversized(kind: &NodeKind) -> bool {
    match kind {
        NodeKind::Branch { conditions } => conditions.len() > MAX_CONDITIONS,
        NodeKind::SplitRandom { arms } => arms.len() > MAX_ARMS,
        _ => false,
    }
}

fn check_edge<'a>(
    edge: &'a Edge,
    index_of: &HashMap<&str, usize>,
    graph: &FlowGraph,
    wired: &mut HashSet<(usize, &'a str)>,
    out: &mut Vec<Issue>,
) {
    if !index_of.contains_key(edge.to.as_str()) {
        out.push(Issue::edge(code::UNKNOWN_NODE, &edge.id).at("/to"));
    }
    let Some(&from) = index_of.get(edge.from.as_str()) else {
        out.push(Issue::edge(code::UNKNOWN_NODE, &edge.id).at("/from"));
        return;
    };
    if !graph.nodes[from].kind.has_port(&edge.port) {
        out.push(Issue::edge(code::UNKNOWN_PORT, &edge.id).at("/port"));
    } else if !wired.insert((from, &edge.port)) {
        out.push(Issue::edge(code::PORT_WIRED_TWICE, &edge.id).at("/port"));
    }
}

/// One entry, no cycle, and every node reachable from it. A repeated node id is already
/// refused; its later nodes take no part in the shape.
fn check_shape(
    graph: &FlowGraph,
    index_of: &HashMap<&str, usize>,
    edges: &[(usize, usize)],
    out: &mut Vec<Issue>,
) {
    let count = graph.nodes.len();
    let first = |node: usize| index_of.get(graph.nodes[node].id.as_str()) == Some(&node);
    let mut incoming = vec![0_usize; count];
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); count];
    for &(from, to) in edges {
        incoming[to] += 1;
        next[from].push(to);
    }
    let roots: Vec<usize> = (0..count)
        .filter(|&node| first(node) && incoming[node] == 0)
        .collect();
    match roots.len() {
        0 => out.push(Issue::general(code::NO_ENTRY)),
        1 => {}
        _ => {
            for &root in &roots {
                out.push(Issue::node(code::MULTIPLE_ENTRIES, &graph.nodes[root].id));
            }
        }
    }

    for (node, _) in on_cycle(&next).iter().enumerate().filter(|(_, on)| **on) {
        out.push(Issue::node(code::CYCLE, &graph.nodes[node].id));
    }

    let mut seen = vec![false; count];
    let mut stack = roots;
    for &root in &stack {
        seen[root] = true;
    }
    while let Some(node) = stack.pop() {
        for &to in &next[node] {
            if !seen[to] {
                seen[to] = true;
                stack.push(to);
            }
        }
    }
    for (node, _) in seen
        .iter()
        .enumerate()
        .filter(|(node, seen)| first(*node) && !**seen)
    {
        out.push(Issue::node(code::UNREACHABLE, &graph.nodes[node].id));
    }
}

/// Which nodes lie on a cycle: those in a strongly connected component of more than one node, or
/// with an edge to themselves. Tarjan's algorithm with an explicit stack, so no recursion.
fn on_cycle(next: &[Vec<usize>]) -> Vec<bool> {
    const UNVISITED: usize = usize::MAX;
    let count = next.len();
    let mut order = vec![UNVISITED; count];
    let mut low = vec![0_usize; count];
    let mut open = vec![false; count];
    let mut stack: Vec<usize> = Vec::new();
    let mut found = vec![false; count];
    let mut clock = 0;
    for start in 0..count {
        if order[start] != UNVISITED {
            continue;
        }
        order[start] = clock;
        low[start] = clock;
        clock += 1;
        stack.push(start);
        open[start] = true;
        // Each frame is a node and how many of its edges are done.
        let mut frames = vec![(start, 0_usize)];
        while let Some(&(node, done)) = frames.last() {
            if let Some(&to) = next[node].get(done) {
                if let Some(frame) = frames.last_mut() {
                    frame.1 += 1;
                }
                if order[to] == UNVISITED {
                    order[to] = clock;
                    low[to] = clock;
                    clock += 1;
                    stack.push(to);
                    open[to] = true;
                    frames.push((to, 0));
                } else if open[to] {
                    low[node] = low[node].min(order[to]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] != order[node] {
                continue;
            }
            let mut members = Vec::new();
            while let Some(member) = stack.pop() {
                open[member] = false;
                members.push(member);
                if member == node {
                    break;
                }
            }
            if members.len() > 1 || next[node].contains(&node) {
                for member in members {
                    found[member] = true;
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::graph::{Arm, ListChange, TraitChange, Weekday};
    use crate::flow::settings::{Audience, SegmentChange};
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed};
    use serde_json::json;
    use uuid::Uuid;

    fn node(id: &str, kind: NodeKind) -> FlowNode {
        FlowNode {
            id: id.into(),
            kind,
        }
    }

    fn edge(id: &str, from: &str, port: &str, to: &str) -> Edge {
        Edge {
            id: id.into(),
            from: from.into(),
            port: port.into(),
            to: to.into(),
        }
    }

    fn graph(nodes: Vec<FlowNode>, edges: Vec<Edge>) -> FlowGraph {
        FlowGraph {
            v: GRAPH_VERSION,
            nodes,
            edges,
        }
    }

    fn wait(seconds: u64) -> NodeKind {
        NodeKind::WaitDuration { seconds }
    }

    /// `a` (a wait) leads to `z` (an exit).
    fn minimal() -> FlowGraph {
        graph(
            vec![node("a", wait(60)), node("z", NodeKind::Exit {})],
            vec![edge("e1", "a", "next", "z")],
        )
    }

    /// `id` of kind `kind`, with every port wired to a node `z` that is an exit, after an entry.
    fn around(kind: NodeKind) -> FlowGraph {
        let mut edges = vec![edge("in", "a", "next", "m")];
        for (index, port) in kind.ports().iter().enumerate() {
            edges.push(edge(&format!("o{index}"), "m", port, "z"));
        }
        graph(
            vec![
                node("a", wait(60)),
                node("m", kind),
                node("z", NodeKind::Exit {}),
            ],
            edges,
        )
    }

    fn event_match() -> EventMatch {
        EventMatch {
            name: "order".into(),
            filters: vec![json!({"prop": "total", "op": "gt", "value": 5})],
        }
    }

    fn settings(trigger: Trigger) -> FlowSettings {
        FlowSettings {
            trigger,
            reentry: Reentry::Once {},
            goal: None,
            exit_on_goal: false,
            exit_criteria: Vec::new(),
        }
    }

    /// The only issue of the graph is `code` on this node or edge.
    #[track_caller]
    fn only(issues: &[Issue], expected: &'static str, node: Option<&str>, edge: Option<&str>) {
        let want = [Issue {
            code: expected,
            node: node.map(str::to_owned),
            edge: edge.map(str::to_owned),
            at: None,
        }];
        let strip: Vec<Issue> = issues
            .iter()
            .map(|issue| Issue {
                at: None,
                ..issue.clone()
            })
            .collect();
        assert_eq!(strip, want, "{issues:?}");
    }

    #[track_caller]
    fn has(issues: &[Issue], expected: &str, node: Option<&str>, edge: Option<&str>) {
        assert!(
            issues.iter().any(|issue| issue.code == expected
                && issue.node.as_deref() == node
                && issue.edge.as_deref() == edge),
            "{expected} {node:?} {edge:?} not in {issues:?}"
        );
    }

    #[test]
    fn the_minimal_graph_and_settings_are_valid() {
        assert_eq!(validate_graph(&minimal()), []);
        assert_eq!(validate_settings(&settings(Trigger::Manual {})), []);
        assert_eq!(validate(&minimal(), &settings(Trigger::Api {})), []);
    }

    #[test]
    fn a_graph_using_every_node_kind_is_valid() {
        let kinds = vec![
            wait(60),
            NodeKind::WaitUntilTime {
                when: UntilTime::TimeOfDay {
                    time: "09:30".into(),
                    weekdays: vec![Weekday::Mon, Weekday::Tue],
                },
            },
            NodeKind::WaitUntilEvent {
                event: event_match(),
                timeout_seconds: 3600,
            },
            NodeKind::Branch {
                conditions: vec![
                    json!({"trait": "plan", "op": "eq", "value": "pro"}),
                    json!({"all": [{"identifier": "email"}, {"not": {"identifier": "phone"}}]}),
                ],
            },
            NodeKind::SplitRandom {
                arms: vec![Arm { weight: 25 }, Arm { weight: 75 }],
            },
            NodeKind::ActionWebhook {
                url: "https://example.com/hook".into(),
                trait_keys: vec!["plan".into(), "country".into()],
                template: None,
            },
            NodeKind::ActionUpdateTrait {
                key: "visits".into(),
                change: TraitChange::Increment { by: 1.0 },
            },
            NodeKind::ActionList {
                list_id: Uuid::nil(),
                change: ListChange::Add,
            },
            NodeKind::Goal { exit: false },
            NodeKind::Goal { exit: true },
        ];
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for (index, kind) in kinds.into_iter().enumerate() {
            let id = format!("n{index}");
            if index > 0 {
                // Each node is entered from the one before; its other ports go to the exit.
                let (from, port) = if index == 1 {
                    ("n0".to_owned(), "next".to_owned())
                } else {
                    let before: &FlowNode = nodes.last().unwrap();
                    (before.id.clone(), before.kind.ports()[0].clone())
                };
                edges.push(edge(&format!("in{index}"), &from, &port, &id));
            }
            for (slot, port) in kind.ports().iter().enumerate().skip(1) {
                edges.push(edge(&format!("x{index}_{slot}"), &id, port, "exit"));
            }
            nodes.push(node(&id, kind));
        }
        nodes.push(node("exit", NodeKind::Exit {}));
        let valid = graph(nodes, edges);
        assert_eq!(validate_graph(&valid), []);
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        let mut bad = minimal();
        bad.v = 2;
        only(&validate_graph(&bad), code::UNSUPPORTED_VERSION, None, None);
    }

    #[test]
    fn too_many_nodes_are_refused() {
        let mut nodes: Vec<FlowNode> = (0..MAX_NODES)
            .map(|index| node(&format!("n{index}"), wait(60)))
            .collect();
        nodes.push(node("z", NodeKind::Exit {}));
        let mut edges: Vec<Edge> = (1..MAX_NODES)
            .map(|index| {
                edge(
                    &format!("e{index}"),
                    &format!("n{}", index - 1),
                    "next",
                    &format!("n{index}"),
                )
            })
            .collect();
        edges.push(edge("last", &format!("n{}", MAX_NODES - 1), "next", "z"));
        assert_eq!(
            validate_graph(&graph(nodes.clone(), edges.clone())),
            [Issue::general(code::TOO_MANY_NODES)]
        );
        nodes.pop();
        nodes.truncate(MAX_NODES - 1);
        edges.truncate(MAX_NODES - 2);
        let within = graph(nodes, edges);
        has(
            &validate_graph(&within),
            code::PORT_NOT_WIRED,
            Some("n198"),
            None,
        );
        assert!(
            !validate_graph(&within)
                .iter()
                .any(|i| i.code == code::TOO_MANY_NODES)
        );
    }

    #[test]
    fn a_bad_node_or_edge_id_is_refused() {
        let mut bad = minimal();
        bad.nodes[0].id = "bad id".into();
        bad.edges[0].from = "bad id".into();
        bad.edges[0].id = String::new();
        let issues = validate_graph(&bad);
        let invalid: Vec<Option<&str>> = issues
            .iter()
            .filter(|i| i.code == code::INVALID_ID)
            .map(|i| i.at.as_deref())
            .collect();
        assert_eq!(invalid, [Some("/nodes/0/id"), Some("/edges/0/id")]);
        assert_eq!(issues.len(), 2, "{issues:?}");
        // The bad id is pointed at, never echoed.
        bad.nodes[0].id = "x".repeat(MAX_ID_CHARS + 1);
        bad.edges[0].from = bad.nodes[0].id.clone();
        bad.edges[0].id = "e1".into();
        let issues = validate_graph(&bad);
        let invalid = Issue::general(code::INVALID_ID).at("/nodes/0/id");
        assert!(issues.contains(&invalid), "{issues:?}");
        assert!(
            !issues
                .iter()
                .any(|i| { i.code == code::INVALID_ID && (i.node.is_some() || i.edge.is_some()) })
        );
    }

    #[test]
    fn a_duplicate_node_id_is_refused() {
        let mut bad = minimal();
        bad.nodes.push(node("z", NodeKind::Exit {}));
        let issues = validate_graph(&bad);
        // The repeated node takes no part in the shape: no second entry, no unreachable node.
        only(&issues, code::DUPLICATE_NODE_ID, Some("z"), None);
    }

    #[test]
    fn a_duplicate_edge_id_is_refused() {
        let bad = graph(
            vec![
                node("a", wait(60)),
                node("b", wait(60)),
                node("z", NodeKind::Exit {}),
            ],
            vec![edge("e1", "a", "next", "b"), edge("e1", "b", "next", "z")],
        );
        only(
            &validate_graph(&bad),
            code::DUPLICATE_EDGE_ID,
            None,
            Some("e1"),
        );
    }

    #[test]
    fn an_edge_to_or_from_an_unknown_node_is_refused() {
        let to = graph(
            vec![node("a", wait(60))],
            vec![edge("e1", "a", "next", "ghost")],
        );
        only(&validate_graph(&to), code::UNKNOWN_NODE, None, Some("e1"));
        let from = graph(
            vec![node("z", NodeKind::Exit {})],
            vec![edge("e1", "ghost", "next", "z")],
        );
        only(&validate_graph(&from), code::UNKNOWN_NODE, None, Some("e1"));
    }

    #[test]
    fn an_edge_out_of_a_port_the_node_lacks_is_refused() {
        let bad = graph(
            vec![
                node("a", wait(60)),
                node("z", NodeKind::Exit {}),
                node("y", NodeKind::Exit {}),
            ],
            vec![edge("e1", "a", "next", "z"), edge("e2", "z", "next", "y")],
        );
        only(&validate_graph(&bad), code::UNKNOWN_PORT, None, Some("e2"));
        let misnamed = graph(
            vec![node("a", wait(60)), node("z", NodeKind::Exit {})],
            vec![edge("e1", "a", "else", "z")],
        );
        let issues = validate_graph(&misnamed);
        has(&issues, code::UNKNOWN_PORT, None, Some("e1"));
        has(&issues, code::PORT_NOT_WIRED, Some("a"), None);
    }

    #[test]
    fn a_port_wired_twice_is_refused() {
        let bad = graph(
            vec![
                node("a", wait(60)),
                node("z", NodeKind::Exit {}),
                node("y", NodeKind::Exit {}),
            ],
            vec![edge("e1", "a", "next", "z"), edge("e2", "a", "next", "y")],
        );
        only(
            &validate_graph(&bad),
            code::PORT_WIRED_TWICE,
            None,
            Some("e2"),
        );
    }

    #[test]
    fn a_required_port_left_unwired_is_refused() {
        let bad = graph(
            vec![
                node(
                    "w",
                    NodeKind::WaitUntilEvent {
                        event: event_match(),
                        timeout_seconds: 60,
                    },
                ),
                node("z", NodeKind::Exit {}),
            ],
            vec![edge("e1", "w", "event", "z")],
        );
        let issues = validate_graph(&bad);
        only(&issues, code::PORT_NOT_WIRED, Some("w"), None);
        assert_eq!(issues[0].at.as_deref(), Some("/ports/timeout"));
    }

    #[test]
    fn a_graph_without_nodes_has_no_entry() {
        only(
            &validate_graph(&graph(vec![], vec![])),
            code::NO_ENTRY,
            None,
            None,
        );
    }

    #[test]
    fn more_than_one_entry_is_refused() {
        let bad = graph(
            vec![
                node("a", wait(60)),
                node("b", wait(60)),
                node("z", NodeKind::Exit {}),
            ],
            vec![edge("e1", "a", "next", "z"), edge("e2", "b", "next", "z")],
        );
        let issues = validate_graph(&bad);
        assert_eq!(issues.len(), 2, "{issues:?}");
        has(&issues, code::MULTIPLE_ENTRIES, Some("a"), None);
        has(&issues, code::MULTIPLE_ENTRIES, Some("b"), None);
    }

    #[test]
    fn a_cycle_is_refused_and_only_its_nodes_are_named() {
        let bad = graph(
            vec![
                node("s", wait(60)),
                node(
                    "w",
                    NodeKind::WaitUntilEvent {
                        event: event_match(),
                        timeout_seconds: 60,
                    },
                ),
                node("z", NodeKind::Exit {}),
            ],
            vec![
                edge("e1", "s", "next", "w"),
                edge("e2", "w", "event", "z"),
                edge("e3", "w", "timeout", "w"),
            ],
        );
        only(&validate_graph(&bad), code::CYCLE, Some("w"), None);
    }

    #[test]
    fn a_node_between_two_cycles_is_not_named() {
        // s -> p <-> q -> m -> r <-> t -> z: m lies on a path between the cycles, not on one.
        let branch = |id: &str| {
            node(
                id,
                NodeKind::Branch {
                    conditions: vec![json!({"identifier": "email"})],
                },
            )
        };
        let bad = graph(
            vec![
                node("s", wait(60)),
                branch("p"),
                node("q", wait(60)),
                node("m", wait(60)),
                branch("r"),
                node("t", wait(60)),
                node("z", NodeKind::Exit {}),
            ],
            vec![
                edge("e1", "s", "next", "p"),
                edge("e2", "p", "case_0", "q"),
                edge("e3", "q", "next", "p"),
                edge("e4", "p", "else", "m"),
                edge("e5", "m", "next", "r"),
                edge("e6", "r", "case_0", "t"),
                edge("e7", "t", "next", "r"),
                edge("e8", "r", "else", "z"),
            ],
        );
        let named: Vec<&str> = validate_graph(&bad)
            .iter()
            .filter(|i| i.code == code::CYCLE)
            .filter_map(|i| i.node.as_deref())
            .map(|id| {
                ["p", "q", "r", "t"]
                    .into_iter()
                    .find(|n| *n == id)
                    .unwrap_or("?")
            })
            .collect();
        assert_eq!(named, ["p", "q", "r", "t"]);
    }

    #[test]
    fn a_node_nothing_leads_to_is_unreachable() {
        let mut bad = minimal();
        bad.nodes.push(node("p", wait(60)));
        bad.nodes.push(node("q", wait(60)));
        bad.edges.push(edge("e2", "p", "next", "q"));
        bad.edges.push(edge("e3", "q", "next", "p"));
        let issues = validate_graph(&bad);
        has(&issues, code::UNREACHABLE, Some("p"), None);
        has(&issues, code::UNREACHABLE, Some("q"), None);
        has(&issues, code::CYCLE, Some("p"), None);
    }

    #[test]
    fn a_wait_must_be_positive_and_below_the_maximum() {
        for seconds in [0, MAX_DURATION_SECONDS + 1] {
            only(
                &validate_graph(&around(wait(seconds))),
                code::INVALID_DURATION,
                Some("m"),
                None,
            );
            let timeout = NodeKind::WaitUntilEvent {
                event: event_match(),
                timeout_seconds: seconds,
            };
            only(
                &validate_graph(&around(timeout)),
                code::INVALID_DURATION,
                Some("m"),
                None,
            );
        }
        assert_eq!(validate_graph(&around(wait(MAX_DURATION_SECONDS))), []);
        assert_eq!(validate_graph(&around(wait(1))), []);
    }

    #[test]
    fn an_until_time_must_parse() {
        for when in [
            UntilTime::TimeOfDay {
                time: "25:00".into(),
                weekdays: vec![],
            },
            UntilTime::TimeOfDay {
                time: "9:30".into(),
                weekdays: vec![],
            },
            UntilTime::DateTime {
                at: "2030-13-01T00:00:00".into(),
            },
            UntilTime::DateTime {
                at: "2030-01-01T00:00:00Z".into(),
            },
            UntilTime::DateTime {
                at: "2030-12-31T23:59:60".into(),
            },
            UntilTime::DateTime {
                at: "2030-1-1T00:00:00".into(),
            },
            UntilTime::TimeOfDay {
                time: "+9:30".into(),
                weekdays: vec![],
            },
            UntilTime::TimeOfDay {
                time: "09-30".into(),
                weekdays: vec![],
            },
        ] {
            let kind = NodeKind::WaitUntilTime { when };
            only(
                &validate_graph(&around(kind)),
                code::INVALID_TIME,
                Some("m"),
                None,
            );
        }
        let good = NodeKind::WaitUntilTime {
            when: UntilTime::DateTime {
                at: "2030-01-01T00:00:00".into(),
            },
        };
        assert_eq!(validate_graph(&around(good)), []);
    }

    #[test]
    fn repeated_weekdays_are_refused() {
        let kind = NodeKind::WaitUntilTime {
            when: UntilTime::TimeOfDay {
                time: "09:00".into(),
                weekdays: vec![Weekday::Mon, Weekday::Mon],
            },
        };
        only(
            &validate_graph(&around(kind)),
            code::INVALID_WEEKDAYS,
            Some("m"),
            None,
        );
    }

    #[test]
    fn an_empty_event_name_is_refused() {
        let kind = NodeKind::WaitUntilEvent {
            event: EventMatch {
                name: " ".into(),
                filters: vec![],
            },
            timeout_seconds: 60,
        };
        only(
            &validate_graph(&around(kind)),
            code::INVALID_EVENT_NAME,
            Some("m"),
            None,
        );
    }

    #[test]
    fn an_event_filter_the_segment_parser_refuses_is_refused() {
        let kind = NodeKind::WaitUntilEvent {
            event: EventMatch {
                name: "order".into(),
                filters: vec![json!({"prop": "total", "op": "nope"})],
            },
            timeout_seconds: 60,
        };
        let issues = validate_graph(&around(kind));
        has(&issues, code::INVALID_FILTER, Some("m"), None);
        assert!(
            issues[0].at.as_deref().unwrap().starts_with("/filters/0"),
            "{issues:?}"
        );
    }

    #[test]
    fn a_branch_needs_a_condition() {
        let kind = NodeKind::Branch { conditions: vec![] };
        only(
            &validate_graph(&around(kind)),
            code::NO_CONDITIONS,
            Some("m"),
            None,
        );
    }

    #[test]
    fn a_branch_has_at_most_the_limit_of_conditions() {
        let condition = json!({"identifier": "email"});
        let kind = NodeKind::Branch {
            conditions: vec![condition.clone(); MAX_CONDITIONS + 1],
        };
        only(
            &validate_graph(&around(kind)),
            code::TOO_MANY_CONDITIONS,
            Some("m"),
            None,
        );
        let kind = NodeKind::Branch {
            conditions: vec![condition; MAX_CONDITIONS],
        };
        assert_eq!(validate_graph(&around(kind)), []);
    }

    #[test]
    fn a_condition_the_segment_parser_refuses_is_refused() {
        for condition in [
            json!({"trait": "plan", "op": "bogus"}),
            json!({"nonsense": 1}),
            json!(5),
            json!({"v": 1, "identifier": "email"}),
        ] {
            let kind = NodeKind::Branch {
                conditions: vec![condition],
            };
            let issues = validate_graph(&around(kind));
            has(&issues, code::INVALID_CONDITION, Some("m"), None);
            assert!(
                issues[0]
                    .at
                    .as_deref()
                    .unwrap()
                    .starts_with("/conditions/0"),
                "{issues:?}"
            );
        }
    }

    #[test]
    fn a_split_needs_two_arms() {
        let kind = NodeKind::SplitRandom {
            arms: vec![Arm { weight: 100 }],
        };
        only(
            &validate_graph(&around(kind)),
            code::TOO_FEW_ARMS,
            Some("m"),
            None,
        );
    }

    #[test]
    fn a_split_has_at_most_the_limit_of_arms() {
        let mut arms = vec![Arm { weight: 10 }; MAX_ARMS];
        // Ten arms of 10 are allowed; ten of 9 and one of 10 are eleven arms that sum to 100.
        assert_eq!(
            validate_graph(&around(NodeKind::SplitRandom { arms: arms.clone() })),
            []
        );
        arms = vec![Arm { weight: 9 }; MAX_ARMS];
        arms.push(Arm { weight: 10 });
        only(
            &validate_graph(&around(NodeKind::SplitRandom { arms })),
            code::TOO_MANY_ARMS,
            Some("m"),
            None,
        );
    }

    #[test]
    fn a_zero_weight_arm_is_refused() {
        let kind = NodeKind::SplitRandom {
            arms: vec![Arm { weight: 0 }, Arm { weight: 100 }],
        };
        only(
            &validate_graph(&around(kind)),
            code::INVALID_WEIGHT,
            Some("m"),
            None,
        );
    }

    #[test]
    fn split_weights_must_sum_to_100() {
        for weights in [[50, 40], [60, 50]] {
            let arms = weights.map(|weight| Arm { weight }).to_vec();
            let kind = NodeKind::SplitRandom { arms };
            only(
                &validate_graph(&around(kind)),
                code::WEIGHTS_NOT_100,
                Some("m"),
                None,
            );
        }
        let ok = NodeKind::SplitRandom {
            arms: vec![Arm { weight: 99 }, Arm { weight: 1 }],
        };
        assert_eq!(validate_graph(&around(ok)), []);
    }

    #[test]
    fn a_webhook_needs_an_http_url() {
        for url in [
            "ftp://example.com",
            "https://",
            "https://@",
            "https://user@",
            "https://user:pw@/x",
            "https:///x",
            "https://a b",
            "example.com",
            "",
        ] {
            let kind = NodeKind::ActionWebhook {
                url: url.into(),
                trait_keys: vec![],
                template: None,
            };
            only(
                &validate_graph(&around(kind)),
                code::INVALID_URL,
                Some("m"),
                None,
            );
        }
        for url in ["http://example.com", "HTTPS://example.com:8443/a?b=c"] {
            let kind = NodeKind::ActionWebhook {
                url: url.into(),
                trait_keys: vec![],
                template: None,
            };
            assert_eq!(validate_graph(&around(kind)), [], "{url}");
        }
    }

    #[test]
    fn a_trait_key_must_be_named_and_listed_once() {
        for keys in [vec!["".to_owned()], vec!["a".to_owned(), "a".to_owned()]] {
            let kind = NodeKind::ActionWebhook {
                url: "https://example.com".into(),
                trait_keys: keys,
                template: None,
            };
            only(
                &validate_graph(&around(kind)),
                code::INVALID_TRAIT_KEY,
                Some("m"),
                None,
            );
        }
        let kind = NodeKind::ActionUpdateTrait {
            key: " ".into(),
            change: TraitChange::Unset {},
        };
        only(
            &validate_graph(&around(kind)),
            code::INVALID_TRAIT_KEY,
            Some("m"),
            None,
        );
    }

    #[test]
    fn a_trait_value_is_bounded_and_an_increment_is_finite() {
        let deep = (0..segment::MAX_DEPTH + 1).fold(json!(1), |inner, _| json!([inner]));
        let wide = Value::Array(vec![json!(0); segment::MAX_NODES + 1]);
        let long = Value::String("x".repeat(MAX_VALUE_STRING_CHARS + 1));
        for value in [deep, wide, long] {
            let kind = NodeKind::ActionUpdateTrait {
                key: "k".into(),
                change: TraitChange::Set { value },
            };
            let issues = validate_graph(&around(kind));
            only(&issues, code::INVALID_TRAIT_VALUE, Some("m"), None);
            assert_eq!(issues[0].at.as_deref(), Some("/change/value"));
        }
        let fine =
            json!({"list": [1, "two", {"three": null}], "ok": "x".repeat(MAX_VALUE_STRING_CHARS)});
        let kind = NodeKind::ActionUpdateTrait {
            key: "k".into(),
            change: TraitChange::Set { value: fine },
        };
        assert_eq!(validate_graph(&around(kind)), []);
        for by in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let kind = NodeKind::ActionUpdateTrait {
                key: "k".into(),
                change: TraitChange::Increment { by },
            };
            let issues = validate_graph(&around(kind));
            only(&issues, code::INVALID_TRAIT_VALUE, Some("m"), None);
            assert_eq!(issues[0].at.as_deref(), Some("/change/by"));
        }
    }

    #[test]
    fn a_graph_over_the_edge_limit_is_refused_before_anything_else() {
        let mut bad = minimal();
        bad.edges = (0..=MAX_EDGES)
            .map(|index| edge(&format!("e{index}"), "a", "next", "z"))
            .collect();
        assert_eq!(validate_graph(&bad), [Issue::general(code::TOO_MANY_EDGES)]);
    }

    #[test]
    fn a_huge_branch_or_split_yields_bounded_output() {
        let huge = 1_000_000;
        let kinds = [
            NodeKind::Branch {
                conditions: vec![json!({"identifier": "email"}); huge],
            },
            NodeKind::SplitRandom {
                arms: vec![Arm { weight: 1 }; huge],
            },
        ];
        for kind in kinds {
            let big = graph(
                vec![node("a", wait(60)), node("m", kind)],
                vec![edge("in", "a", "next", "m")],
            );
            let issues = validate_graph(&big);
            assert!(issues.len() <= MAX_ISSUES, "{}", issues.len());
            assert!(issues.iter().all(|i| i.code != code::PORT_NOT_WIRED));
            // An oversized split is not summed: the arm limit is the only weight issue.
            assert!(issues.iter().all(|i| i.code != code::WEIGHTS_NOT_100));
            assert!(
                issues
                    .iter()
                    .any(|i| i.code == code::TOO_MANY_CONDITIONS || i.code == code::TOO_MANY_ARMS)
            );
            // An edge from a port just past the list is unknown, found without listing ports.
            let mut stray = big.clone();
            stray.edges.push(edge("far", "m", "case_1000000", "a"));
            stray.edges.push(edge("far2", "m", "arm_1000000", "a"));
            let unknown = validate_graph(&stray)
                .iter()
                .filter(|i| i.code == code::UNKNOWN_PORT)
                .count();
            assert_eq!(unknown, 2);
        }
        // Many conditions inside the limits of nodes and edges: the issue list is capped.
        let nodes: Vec<FlowNode> = (0..MAX_NODES)
            .map(|index| node(&format!("n{index}"), wait(60)))
            .collect();
        let capped = validate_graph(&graph(nodes, vec![]));
        assert_eq!(capped.len(), MAX_ISSUES);
    }

    #[test]
    fn the_combined_validation_is_capped_too() {
        let nodes: Vec<FlowNode> = (0..MAX_NODES)
            .map(|index| node(&format!("n{index}"), wait(60)))
            .collect();
        let mut flow = settings(Trigger::Manual {});
        flow.exit_criteria = vec![ExitCriterion::GoalReached {}; 500];
        let issues = validate(&graph(nodes, vec![]), &flow);
        assert_eq!(issues.len(), MAX_ISSUES);
        // Only the criteria within the limit are checked, so the settings alone stay small.
        assert_eq!(validate_settings(&flow).len(), 1 + MAX_EXIT_CRITERIA);
    }

    #[test]
    fn a_flow_has_at_most_the_limit_of_exit_criteria() {
        let event = ExitCriterion::Event(EventMatch {
            name: "order".into(),
            filters: vec![],
        });
        let mut flow = settings(Trigger::Manual {});
        flow.exit_criteria = vec![event.clone(); MAX_EXIT_CRITERIA];
        assert_eq!(validate_settings(&flow), []);
        flow.exit_criteria.push(event);
        only(
            &validate_settings(&flow),
            code::TOO_MANY_EXIT_CRITERIA,
            None,
            None,
        );
    }

    #[test]
    fn a_webhook_sends_at_most_the_limit_of_trait_keys() {
        let keys = (0..=MAX_TRAIT_KEYS)
            .map(|index| format!("k{index}"))
            .collect();
        let kind = NodeKind::ActionWebhook {
            url: "https://example.com".into(),
            trait_keys: keys,
            template: None,
        };
        only(
            &validate_graph(&around(kind)),
            code::TOO_MANY_TRAIT_KEYS,
            Some("m"),
            None,
        );
    }

    #[test]
    fn a_cron_that_does_not_parse_is_refused() {
        let trigger = Trigger::Schedule {
            cron: "not a cron".into(),
            timezone: "UTC".into(),
            audience: Audience::Everyone {},
        };
        only(
            &validate_settings(&settings(trigger)),
            code::INVALID_CRON,
            None,
            None,
        );
    }

    #[test]
    fn a_time_zone_that_does_not_parse_is_refused() {
        let trigger = Trigger::Schedule {
            cron: "0 9 * * 1".into(),
            timezone: "Mars/Base".into(),
            audience: Audience::Everyone {},
        };
        only(
            &validate_settings(&settings(trigger)),
            code::INVALID_TIMEZONE,
            None,
            None,
        );
        let good = Trigger::Schedule {
            cron: "0 9 * * 1".into(),
            timezone: "Europe/Copenhagen".into(),
            audience: Audience::Everyone {},
        };
        assert_eq!(validate_settings(&settings(good)), []);
    }

    #[test]
    fn an_event_trigger_goal_and_criterion_are_checked_like_a_wait() {
        let bad = EventMatch {
            name: "order".into(),
            filters: vec![json!({"prop": "p", "op": "nope"})],
        };
        let mut flow = settings(Trigger::Event(bad.clone()));
        flow.goal = Some(Goal::Event(bad.clone()));
        flow.exit_criteria = vec![ExitCriterion::Event(bad)];
        let issues = validate_settings(&flow);
        let at: Vec<&str> = issues.iter().filter_map(|i| i.at.as_deref()).collect();
        assert!(
            at.iter().any(|p| p.starts_with("/trigger/filters")),
            "{at:?}"
        );
        assert!(at.iter().any(|p| p.starts_with("/goal/filters")), "{at:?}");
        assert!(
            at.iter().any(|p| p.starts_with("/exit_criteria/0/filters")),
            "{at:?}"
        );
        assert!(issues.iter().all(|i| i.code == code::INVALID_FILTER));
    }

    #[test]
    fn a_reentry_interval_above_the_maximum_is_refused() {
        let mut flow = settings(Trigger::Manual {});
        flow.reentry = Reentry::Always {
            min_interval_seconds: MAX_DURATION_SECONDS + 1,
        };
        only(
            &validate_settings(&flow),
            code::INVALID_DURATION,
            None,
            None,
        );
        flow.reentry = Reentry::Always {
            min_interval_seconds: 0,
        };
        assert_eq!(validate_settings(&flow), []);
    }

    #[test]
    fn exit_on_goal_needs_a_goal() {
        let mut flow = settings(Trigger::Manual {});
        flow.exit_on_goal = true;
        only(
            &validate_settings(&flow),
            code::EXIT_ON_GOAL_WITHOUT_GOAL,
            None,
            None,
        );
        flow.goal = Some(Goal::Segment {
            segment_id: Uuid::nil(),
        });
        assert_eq!(validate_settings(&flow), []);
    }

    #[test]
    fn a_goal_criterion_needs_a_goal() {
        let mut flow = settings(Trigger::Manual {});
        flow.exit_criteria = vec![ExitCriterion::GoalReached {}];
        only(
            &validate_settings(&flow),
            code::GOAL_CRITERION_WITHOUT_GOAL,
            None,
            None,
        );
    }

    #[test]
    fn leaving_the_segment_needs_a_segment_trigger() {
        let mut flow = settings(Trigger::Manual {});
        flow.exit_criteria = vec![ExitCriterion::LeavesSegment {}];
        only(
            &validate_settings(&flow),
            code::LEAVES_SEGMENT_WITHOUT_SEGMENT_TRIGGER,
            None,
            None,
        );
        flow.trigger = Trigger::Segment {
            segment_id: Uuid::nil(),
            on: SegmentChange::Entered,
            enroll_current: false,
        };
        assert_eq!(validate_settings(&flow), []);
    }

    // The property test: random DAGs (architecture section 18). The seed is fixed so CI runs the
    // same cases every time; the nightly run sets `ZIRV_FLOW_PROPTEST_SEED` at random.

    const SEED_VAR: &str = "ZIRV_FLOW_PROPTEST_SEED";
    const CASES_VAR: &str = "ZIRV_FLOW_PROPTEST_CASES";
    const DEFAULT_SEED: u64 = 0x5eed_2026_1018;
    const DEFAULT_CASES: u32 = 128;

    fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
        std::env::var(name)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(default)
    }

    fn config() -> Config {
        let seed = env_number(SEED_VAR, DEFAULT_SEED);
        let cases = env_number(CASES_VAR, DEFAULT_CASES);
        println!("flow graph property: seed {seed}, {cases} cases");
        Config {
            cases,
            rng_seed: RngSeed::Fixed(seed),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// A fully wired DAG with one entry and one exit. Node `i` is entered from a random open
    /// port of the nodes before it, so every node is reachable and no edge points backward; the
    /// ports still open at the end all lead to the exit.
    fn build(choices: &[(u8, u32)]) -> FlowGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut open: Vec<(String, String)> = Vec::new();
        let connect = |edges: &mut Vec<Edge>, from: (String, String), to: &str| {
            let id = format!("e{}", edges.len());
            edges.push(edge(&id, &from.0, &from.1, to));
        };
        for (index, &(kind, pick)) in choices.iter().enumerate() {
            let id = format!("n{index}");
            let kind = match kind % 6 {
                0 => wait(60),
                1 => NodeKind::WaitUntilEvent {
                    event: event_match(),
                    timeout_seconds: 60,
                },
                2 => NodeKind::Branch {
                    conditions: vec![json!({"identifier": "email"})],
                },
                3 => NodeKind::SplitRandom {
                    arms: vec![Arm { weight: 40 }, Arm { weight: 60 }],
                },
                // A goal that does not end the enrollment has one port, like any mid-graph node.
                4 => NodeKind::Goal { exit: false },
                _ => NodeKind::ActionList {
                    list_id: Uuid::nil(),
                    change: ListChange::Add,
                },
            };
            if index > 0 {
                let from = open.swap_remove(pick as usize % open.len());
                connect(&mut edges, from, &id);
            }
            open.extend(kind.ports().into_iter().map(|port| (id.clone(), port)));
            nodes.push(node(&id, kind));
        }
        nodes.push(node("exit", NodeKind::Exit {}));
        for from in open {
            connect(&mut edges, from, "exit");
        }
        graph(nodes, edges)
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn every_acyclic_fully_wired_graph_validates(
            choices in proptest::collection::vec((any::<u8>(), any::<u32>()), 1..40),
        ) {
            let built = build(&choices);
            prop_assert_eq!(validate_graph(&built), vec![]);
        }

        #[test]
        fn a_back_edge_makes_a_graph_invalid_with_the_cycle_code(
            choices in proptest::collection::vec((any::<u8>(), any::<u32>()), 1..40),
            pick in any::<u32>(),
        ) {
            let mut built = build(&choices);
            let chosen = built.edges[pick as usize % built.edges.len()].clone();
            built.edges.push(edge("back", &chosen.to, "next", &chosen.from));
            let issues = validate_graph(&built);
            prop_assert!(
                issues.iter().any(|i| i.code == code::CYCLE && i.node.as_deref() == Some(&chosen.from)),
                "{:?}",
                issues
            );
        }
    }
}
