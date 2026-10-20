//! The four invariants, checked from the tables once the run is over, and from the log of the
//! mock destination.
//!
//! 1. **No internal effect applied twice.** Per enrollment, the nodes with a `done` step execution
//!    are replayed in graph order (an edge only leads to a later node, so the index is a
//!    topological order and the enrollment's walk is a chain). Goals leave one `flow.goal`
//!    decision per done goal step. Trait writes are replayed over the person's start (an
//!    increment is not idempotent, so a repeated one shows in the value, and a repeated set after
//!    a later write shows too). List writes are replayed against the membership and the
//!    `list_member_changes` rows of source `flow`. Those end states cannot show a repeated
//!    idempotent write (a set, an unset, an add to a member), so triggers on the test database
//!    log every committed write attempt (see `world::install_write_log`) and the attempts per
//!    person, list and operation must equal what the done steps make, once each.
//! 2. **Every external effect enqueued once, under its stable key.** The trigger log of
//!    `webhook.call` inserts holds exactly one row for the key of each done webhook step, and the
//!    destination saw only those keys, with one body per key however often it was called.
//! 3. **No wake before its `wake_at`.** The driver recorded the virtual time of every wake that
//!    moved an enrollment on.
//! 4. **End state.** Every enrollment ended, or waits at a wait node with its wake job (and, at
//!    an event wait, its registered wait with the same timeout), or its timer is still in the
//!    future.

use std::collections::{BTreeMap, BTreeSet};

use hashbrown::HashMap;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::flow::{FlowGraph, ListChange, NODE_TYPES, NodeKind, TraitChange};
use zirv_flow_engine::effect_key;
use zirv_flow_store::{STEP_JOB_KIND, WEBHOOK_JOB_KIND};

use super::driver::{Driver, KINDS, WakeMove};
use super::world::{TRAIT_KEYS, World, end};

/// What the checks found.
#[derive(Debug, Default)]
pub(super) struct Findings {
    /// What went wrong during the run itself, a few of them.
    pub(super) run_problems: Vec<String>,
    /// What the tables show afterwards.
    pub(super) violations: Vec<String>,
    pub(super) summary: BTreeMap<String, usize>,
}

impl Findings {
    /// Records a violation. The first few of each invariant are kept for the report; all are
    /// counted, in total and under the invariant's name.
    fn violate(&mut self, text: String) {
        let category = text.split(':').next().unwrap_or("other").to_owned();
        let seen = self
            .summary
            .entry(format!("violations of {category}"))
            .or_default();
        *seen += 1;
        if *seen <= 8 {
            self.violations.push(text);
        }
        *self.summary.entry("violations".to_owned()).or_default() += 1;
    }

    fn count(&mut self, key: &str, by: usize) {
        *self.summary.entry(key.to_owned()).or_default() += by;
    }
}

#[derive(sqlx::FromRow)]
struct EnrollmentRow {
    id: Uuid,
    person: Uuid,
    flow: Uuid,
    state: String,
    node: String,
    wake_at: Option<OffsetDateTime>,
    version: i64,
}

/// The index of a generated node id, `n7` being 7.
fn index_of(node: &str) -> usize {
    node.strip_prefix('n')
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(usize::MAX)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

pub(super) async fn check(w: &World, driver: &Driver<'_>, population: usize) -> Findings {
    let mut found = Findings::default();
    let pool = &w.env.pool;
    let ws = w.env.ws.as_uuid();

    for problem in &driver.problems {
        if found.run_problems.len() < 12 {
            found.run_problems.push(problem.clone());
        }
        *found.summary.entry("violations".to_owned()).or_default() += 1;
    }

    let enrollments: Vec<EnrollmentRow> = sqlx::query_as(
        "SELECT id, person_id AS person, flow_id AS flow, state, node, wake_at, \n                state_version AS version \n         FROM enrollments WHERE workspace_id = $1 ORDER BY id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    found.count("enrollments", enrollments.len());
    if enrollments.len() != w.people.len() {
        found.violate(format!(
            "{} people but {} enrollments: an enroll job was lost or repeated",
            w.people.len(),
            enrollments.len()
        ));
    }

    let graphs: HashMap<Uuid, &FlowGraph> =
        w.flows.iter().map(|flow| (flow.id, &flow.graph)).collect();

    // The step executions: every node a walk passed.
    let steps: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT id, enrollment_id, node_id, outcome FROM step_executions WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut done: HashMap<Uuid, Vec<(usize, String, Uuid)>> = HashMap::new();
    for (id, enrollment, node, outcome) in steps {
        if outcome == "done" {
            done.entry(enrollment)
                .or_default()
                .push((index_of(&node), node, id));
        }
    }
    for nodes in done.values_mut() {
        nodes.sort();
    }

    let traits: HashMap<Uuid, Value> =
        sqlx::query_as::<_, (Uuid, Value)>("SELECT id, traits FROM people WHERE workspace_id = $1")
            .bind(ws)
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    let list_changes: Vec<(Uuid, Uuid, String, i64)> = sqlx::query_as(
        "SELECT person_id, list_id, action, count(*) FROM list_member_changes \
         WHERE workspace_id = $1 AND source = 'flow' GROUP BY 1, 2, 3",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    let changes: HashMap<(Uuid, Uuid, String), i64> = list_changes
        .into_iter()
        .map(|(person, list, action, count)| ((person, list, action), count))
        .collect();
    let members: BTreeSet<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT person_id, list_id FROM list_members WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let goal_rows: Vec<(Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT subject_ids ->> 'step_execution', subject_ids ->> 'enrollment', count(*) \
         FROM decision_records WHERE workspace_id = $1 AND action = 'flow.goal' \
         GROUP BY 1, 2",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    let goals: HashMap<String, i64> = goal_rows
        .into_iter()
        .filter_map(|(step, _, count)| step.map(|step| (step, count)))
        .collect();

    // The committed write attempts the triggers logged: (kind, person, target, operation) to count.
    let attempt_rows: Vec<(String, Uuid, String, String, i64)> = sqlx::query_as(
        "SELECT kind, person_id, target, op, count(*) FROM sim_writes \n         WHERE workspace_id = $1 GROUP BY 1, 2, 3, 4",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    let attempts: HashMap<(String, Uuid, String, String), i64> = attempt_rows
        .into_iter()
        .map(|(kind, person, target, op, count)| ((kind, person, target, op), count))
        .collect();
    let mut logged_traits_total = 0_i64;
    let mut logged_inserts_total = 0_i64;
    let mut logged_deletes_total = 0_i64;
    let mut repeated_adds = 0_i64;

    // Invariant 1, and the expected webhook keys for invariant 2.
    let mut expected_keys: BTreeMap<String, Uuid> = BTreeMap::new();
    let mut expected_goals: BTreeSet<String> = BTreeSet::new();
    let mut kinds_run: BTreeSet<&'static str> = BTreeSet::new();
    for enrollment in &enrollments {
        let Some(graph) = graphs.get(&enrollment.flow) else {
            found.violate(format!(
                "enrollment {} names an unknown flow",
                enrollment.id
            ));
            continue;
        };
        let mut values: [Option<f64>; 5] = [None; 5];
        let mut membership: HashMap<Uuid, bool> = HashMap::new();
        let mut expected_changes: HashMap<(Uuid, &'static str), i64> = HashMap::new();
        // The write attempts each done step makes, as the store takes them: a step on a trait is
        // one ledger write, unless it repeats the previous one exactly (the store skips a write
        // whose content hash is the ledger row's); an add is one insert attempt even to a member;
        // a remove is one delete, and only a member has a row to delete.
        let mut trait_writes = 0_i64;
        let mut trait_repeats = 0_i64;
        let mut last_write: Option<(usize, Option<f64>)> = None;
        let mut expected_inserts: HashMap<Uuid, i64> = HashMap::new();
        let mut expected_deletes: HashMap<Uuid, i64> = HashMap::new();
        for (_, node_id, step) in done.get(&enrollment.id).map_or(&[][..], Vec::as_slice) {
            let Some(node) = graph.node(node_id) else {
                found.violate(format!(
                    "enrollment {} ran node {node_id}, which its flow does not have",
                    enrollment.id
                ));
                continue;
            };
            kinds_run.insert(node.kind.type_name());
            match &node.kind {
                NodeKind::ActionUpdateTrait { key, change } => {
                    if let Some(slot) = TRAIT_KEYS.iter().position(|known| known == key) {
                        values[slot] = match change {
                            TraitChange::Set { value } => number(Some(value)),
                            TraitChange::Increment { by } => Some(values[slot].unwrap_or(0.0) + by),
                            TraitChange::Unset {} => None,
                        };
                        let write = (slot, values[slot]);
                        trait_writes += 1;
                        if last_write == Some(write) {
                            trait_repeats += 1;
                        }
                        last_write = Some(write);
                    }
                }
                NodeKind::ActionList { list_id, change } => {
                    let member = membership.entry(*list_id).or_default();
                    match change {
                        ListChange::Add => *expected_inserts.entry(*list_id).or_default() += 1,
                        ListChange::Remove if *member => {
                            *expected_deletes.entry(*list_id).or_default() += 1;
                        }
                        ListChange::Remove => {}
                    }
                    match (change, *member) {
                        (ListChange::Add, false) => {
                            *member = true;
                            *expected_changes.entry((*list_id, "added")).or_default() += 1;
                        }
                        (ListChange::Remove, true) => {
                            *member = false;
                            *expected_changes.entry((*list_id, "removed")).or_default() += 1;
                        }
                        _ => {}
                    }
                }
                NodeKind::Goal { .. } => {
                    expected_goals.insert(step.to_string());
                }
                NodeKind::ActionWebhook { .. } => {
                    expected_keys
                        .insert(effect_key(enrollment.id, node_id, "webhook"), enrollment.id);
                }
                _ => {}
            }
        }

        let person_traits = traits.get(&enrollment.person);
        for (slot, key) in TRAIT_KEYS.iter().enumerate() {
            let actual = number(person_traits.and_then(|held| held.get(*key)));
            let expected = values[slot];
            let same = match (actual, expected) {
                (None, None) => true,
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                _ => false,
            };
            if !same {
                found.violate(format!(
                    "invariant 1: person {} has {key} = {actual:?}, replaying the {} steps of \
                     enrollment {} gives {expected:?}",
                    enrollment.person,
                    done.get(&enrollment.id).map_or(0, Vec::len),
                    enrollment.id
                ));
            }
        }
        for list in &w.lists {
            for action in ["added", "removed"] {
                let actual = changes
                    .get(&(enrollment.person, *list, action.to_owned()))
                    .copied()
                    .unwrap_or(0);
                let expected = expected_changes.get(&(*list, action)).copied().unwrap_or(0);
                if actual != expected {
                    found.violate(format!(
                        "invariant 1: person {} has {actual} '{action}' changes of list {list} by \
                         the flow, replaying enrollment {} gives {expected}",
                        enrollment.person, enrollment.id
                    ));
                }
            }
            let actual = members.contains(&(enrollment.person, *list));
            let expected = membership.get(list).copied().unwrap_or(false);
            if actual != expected {
                found.violate(format!(
                    "invariant 1: person {} is {} list {list}, replaying enrollment {} says {}",
                    enrollment.person,
                    if actual { "in" } else { "not in" },
                    enrollment.id,
                    if expected { "in" } else { "not in" }
                ));
            }
        }

        // The replay above compares end states, which a repeated idempotent write cannot
        // change. The write log can see the repeat: the committed attempts must be exactly
        // those the done steps make, once each.
        let logged = |kind: &str, target: &str, op: &str| {
            attempts
                .get(&(
                    kind.to_owned(),
                    enrollment.person,
                    target.to_owned(),
                    op.to_owned(),
                ))
                .copied()
                .unwrap_or(0)
        };
        let logged_traits: i64 = ["INSERT", "UPDATE", "DELETE"]
            .iter()
            .map(|op| logged("trait", "traits", op))
            .sum();
        logged_traits_total += logged_traits;
        if logged_traits > trait_writes || logged_traits < trait_writes - trait_repeats {
            found.violate(format!(
                "invariant 1 (write attempts): person {} had {logged_traits} committed trait \
                 writes by flow {}, the {trait_writes} done trait steps of enrollment {} make \
                 between {} and {trait_writes}",
                enrollment.person,
                enrollment.flow,
                enrollment.id,
                trait_writes - trait_repeats
            ));
        }
        for list in &w.lists {
            let target = list.to_string();
            for (op, expected) in [
                ("INSERT", expected_inserts.get(list).copied().unwrap_or(0)),
                ("DELETE", expected_deletes.get(list).copied().unwrap_or(0)),
            ] {
                let actual = logged("list", &target, op);
                if op == "INSERT" {
                    logged_inserts_total += actual;
                } else {
                    logged_deletes_total += actual;
                }
                if actual != expected {
                    found.violate(format!(
                        "invariant 1 (write attempts): person {} had {actual} committed {op} \
                         attempts on list {list}, the done steps of enrollment {} make {expected}",
                        enrollment.person, enrollment.id
                    ));
                }
            }
            // Adds that found the person a member already: the repeats only the log can see.
            let added = expected_changes
                .get(&(*list, "added"))
                .copied()
                .unwrap_or(0);
            repeated_adds += expected_inserts.get(list).copied().unwrap_or(0) - added;
        }
    }
    found.count(
        "committed trait write attempts",
        usize::try_from(logged_traits_total).unwrap_or(0),
    );
    found.count(
        "committed list insert attempts",
        usize::try_from(logged_inserts_total).unwrap_or(0),
    );
    found.count(
        "committed list delete attempts",
        usize::try_from(logged_deletes_total).unwrap_or(0),
    );
    found.count(
        "list adds that found a member already",
        usize::try_from(repeated_adds).unwrap_or(0),
    );
    for step in &expected_goals {
        let count = goals.get(step).copied().unwrap_or(0);
        if count != 1 {
            found.violate(format!(
                "invariant 1: the goal step {step} has {count} conversion decisions, not one"
            ));
        }
    }
    for (step, count) in &goals {
        if !expected_goals.contains(step) {
            found.violate(format!(
                "invariant 1: {count} conversion decisions name step {step}, which is not a done goal"
            ));
        }
    }
    found.count("goal conversions", expected_goals.len());

    external_effects(w, &mut found, &expected_keys, &enrollments).await;

    // Invariant 3.
    for text in wake_violations(&driver.wake_moves) {
        found.violate(text);
    }
    found.count("wakes that moved an enrollment", driver.wake_moves.len());

    liveness(w, &mut found, &enrollments, &graphs).await;

    if population >= 100 {
        let missing: Vec<&str> = NODE_TYPES
            .iter()
            .copied()
            .filter(|kind| !kinds_run.contains(kind))
            .collect();
        if !missing.is_empty() {
            found.violate(format!("some node kinds never ran: {missing:?}"));
        }
    }
    found.count("node kinds run", kinds_run.len());
    found
}

/// Invariant 2.
async fn external_effects(
    w: &World,
    found: &mut Findings,
    expected: &BTreeMap<String, Uuid>,
    enrollments: &[EnrollmentRow],
) {
    let pool = &w.env.pool;
    let ws = w.env.ws.as_uuid();
    let logged: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT unique_key, payload ->> 'idempotency_key' FROM sim_enqueued \
         WHERE workspace_id = $1 AND kind = $2",
    )
    .bind(ws)
    .bind(WEBHOOK_JOB_KIND)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut enqueued: BTreeMap<String, usize> = BTreeMap::new();
    for (unique, idempotency) in logged {
        let key = idempotency.unwrap_or_default();
        if unique.as_deref() != Some(key.as_str()) {
            found.violate(format!(
                "invariant 2: the job of {key} has unique key {unique:?}: it is not queued under its stable key"
            ));
        }
        *enqueued.entry(key).or_default() += 1;
    }
    for (key, count) in &enqueued {
        if !expected.contains_key(key) {
            found.violate(format!(
                "invariant 2 (unexpected key): {count} webhook jobs were enqueued under {key}, which no done step explains"
            ));
        } else if *count != 1 {
            found.violate(format!(
                "invariant 2 (enqueued more than once): the webhook of {key} was enqueued {count} times"
            ));
        }
    }
    for key in expected.keys() {
        if !enqueued.contains_key(key) {
            found.violate(format!(
                "invariant 2 (never enqueued): the webhook of {key} was never enqueued"
            ));
        }
    }
    found.count("webhook jobs enqueued", enqueued.len());

    let pending: BTreeSet<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT unique_key FROM jobs WHERE workspace_id = $1 AND kind = $2",
    )
    .bind(ws)
    .bind(WEBHOOK_JOB_KIND)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .flatten()
    .collect();
    let exited: BTreeSet<Uuid> = enrollments
        .iter()
        .filter(|row| row.state == "exited")
        .map(|row| row.id)
        .collect();
    let mut bodies: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    for hit in w.mock.hits() {
        bodies.entry(hit.key).or_default().push(hit.body);
    }
    let mut resends = 0;
    for (key, sent) in &bodies {
        if !expected.contains_key(key) {
            found.violate(format!(
                "invariant 2: the destination saw unexpected key {key}"
            ));
        }
        resends += sent.len() - 1;
        if sent.iter().any(|body| body != &sent[0]) {
            found.violate(format!(
                "invariant 2: the retries of {key} did not send the same body"
            ));
        }
    }
    for (key, enrollment) in expected {
        let delivered = bodies.contains_key(key);
        if !delivered && !pending.contains(key) && !exited.contains(enrollment) {
            found.violate(format!(
                "invariant 2: the webhook of {key} finished without the destination seeing it"
            ));
        }
    }
    // Informational: a call that ran again after a commit without the ack records its outcome
    // again (the product documents the call as at-least-once).
    let (outcomes, distinct): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT (properties ->> 'enrollment_id', properties ->> 'node_id')) \n         FROM events WHERE workspace_id = $1 AND name IN ('webhook.succeeded', 'webhook.failed')",
    )
    .bind(ws)
    .fetch_one(pool)
    .await
    .unwrap();
    found.count(
        "webhook outcome events",
        usize::try_from(outcomes).unwrap_or(0),
    );
    found.count(
        "webhook outcome events beyond one per call",
        usize::try_from(outcomes - distinct).unwrap_or(0),
    );
    found.count("keys the destination saw", bodies.len());
    found.count("calls sent again with the same key", resends);
}

/// Invariant 4.
async fn liveness(
    w: &World,
    found: &mut Findings,
    enrollments: &[EnrollmentRow],
    graphs: &HashMap<Uuid, &FlowGraph>,
) {
    let pool = &w.env.pool;
    let ws = w.env.ws.as_uuid();
    let stop = end();
    let jobs: Vec<(String, String, OffsetDateTime, Value)> = sqlx::query_as(
        "SELECT kind, state, run_at, payload FROM jobs \
         WHERE workspace_id = $1 AND kind = ANY($2)",
    )
    .bind(ws)
    .bind(KINDS.as_slice())
    .fetch_all(pool)
    .await
    .unwrap();
    let mut left: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for (kind, state, run_at, _) in &jobs {
        if state != "ready" {
            *left.entry((kind.clone(), "not ready")).or_default() += 1;
        } else if *run_at <= stop {
            *left.entry((kind.clone(), "due and never run")).or_default() += 1;
        }
    }
    for ((kind, what), count) in left {
        found.violate(format!(
            "invariant 4: {count} {kind} jobs are {what} at the end"
        ));
    }
    let wakes: Vec<(&Value, OffsetDateTime)> = jobs
        .iter()
        .filter(|(kind, state, _, payload)| {
            kind == STEP_JOB_KIND
                && state == "ready"
                && payload.get("input").and_then(Value::as_str) == Some("wake")
        })
        .map(|(_, _, run_at, payload)| (payload, *run_at))
        .collect();
    let waits: Vec<(Uuid, String, i64, OffsetDateTime)> = sqlx::query_as(
        "SELECT enrollment_id, event_name, wake_version, timeout_at FROM enrollment_waits \
         WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut states: BTreeMap<&str, usize> = BTreeMap::new();
    for row in enrollments {
        *states.entry(row.state.as_str()).or_default() += 1;
        match row.state.as_str() {
            "completed" | "exited" => {
                if waits.iter().any(|wait| wait.0 == row.id) {
                    found.violate(format!(
                        "invariant 4: ended enrollment {} still has a registered wait",
                        row.id
                    ));
                }
            }
            "waiting" => {
                let Some(wake_at) = row.wake_at else {
                    found.violate(format!(
                        "invariant 4: waiting enrollment {} has no wake_at",
                        row.id
                    ));
                    continue;
                };
                if wake_at <= stop {
                    found.violate(format!(
                        "invariant 4: enrollment {} still waits at {} though its wake_at {wake_at} \
                         is past the end",
                        row.id, row.node
                    ));
                }
                let has_job = wakes.iter().any(|(payload, run_at)| {
                    payload.get("enrollment_id").and_then(Value::as_str)
                        == Some(row.id.to_string().as_str())
                        && payload.get("state_version").and_then(Value::as_i64) == Some(row.version)
                        && *run_at >= wake_at
                });
                if !has_job {
                    found.violate(format!(
                        "invariant 4: enrollment {} waits at {} with no wake job for version {} \
                         at or after {wake_at}",
                        row.id, row.node, row.version
                    ));
                }
                let at_event_wait = graphs
                    .get(&row.flow)
                    .and_then(|graph| graph.node(&row.node))
                    .is_some_and(|node| matches!(node.kind, NodeKind::WaitUntilEvent { .. }));
                let mine: Vec<_> = waits.iter().filter(|wait| wait.0 == row.id).collect();
                if at_event_wait {
                    let registered = mine
                        .iter()
                        .any(|wait| wait.2 == row.version && wait.3 == wake_at);
                    if mine.len() != 1 || !registered {
                        found.violate(format!(
                            "invariant 4: enrollment {} waits for an event with {} registered \
                             waits, none matching version {} and timeout {wake_at}",
                            row.id,
                            mine.len(),
                            row.version
                        ));
                    }
                } else if !mine.is_empty() {
                    found.violate(format!(
                        "invariant 4: enrollment {} waits at {} (not an event wait) but has a \
                         registered wait",
                        row.id, row.node
                    ));
                }
            }
            other => found.violate(format!(
                "invariant 4: enrollment {} ended the run {other} at {}",
                row.id, row.node
            )),
        }
    }
    for (state, count) in states {
        found.count(&format!("enrollments {state}"), count);
    }
}

/// Invariant 3 over the wakes that moved an enrollment: each answered a timer that was due. A
/// wake that moved an enrollment with no timer at all is as wrong as one that moved it early.
fn wake_violations(moves: &[WakeMove]) -> Vec<String> {
    moves
        .iter()
        .filter_map(|moved| match moved.wake_at {
            None => Some(format!(
                "invariant 3: a wake moved enrollment {} at {} though it had no wake_at",
                moved.enrollment, moved.at
            )),
            Some(wake_at) if moved.at < wake_at => Some(format!(
                "invariant 3: a wake moved enrollment {} at {} though its wake_at is {wake_at}",
                moved.enrollment, moved.at
            )),
            Some(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn moved(wake_at: Option<OffsetDateTime>, at: OffsetDateTime) -> WakeMove {
        WakeMove {
            enrollment: Uuid::from_u128(1),
            wake_at,
            at,
        }
    }

    #[test]
    fn a_wake_that_moves_an_enrollment_needs_a_timer_that_was_due() {
        let due = datetime!(2030-01-02 0:00 UTC);
        assert!(wake_violations(&[moved(Some(due), due)]).is_empty());
        assert!(wake_violations(&[moved(Some(due), datetime!(2030-01-03 0:00 UTC))]).is_empty());
        let early = wake_violations(&[moved(Some(due), datetime!(2030-01-01 0:00 UTC))]);
        assert_eq!(early.len(), 1);
        assert!(early[0].contains("though its wake_at is"));
        let none = wake_violations(&[moved(None, due)]);
        assert_eq!(none.len(), 1);
        assert!(none[0].contains("had no wake_at"));
    }
}
