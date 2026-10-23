//! The world a run starts from: a workspace with its trait, event and list definitions, the
//! generated flows published, the people, the mock destination and the `flow.enroll` jobs that
//! enroll everybody over the first days.

use std::sync::Arc;

use serde_json::json;
use sqlx::PgPool;
use time::Duration as Span;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::events::{WEBHOOK_FAILED_EVENT, WEBHOOK_SUCCEEDED_EVENT};
use zirv_flow_core::flow::{
    Edge, EventMatch, FlowGraph, FlowNode, ListChange, NodeKind, PORT_EVENT, PORT_NEXT,
    PORT_TIMEOUT, validate_graph,
};
use zirv_flow_core::people::TraitType;
use zirv_flow_core::signature::generate_secret;
use zirv_flow_core::{
    Cause, Keyring, MasterKey, SecretId, SecretPurpose, Slug, TraceContext, WorkspaceName, seal,
    secret_aad,
};
use zirv_flow_engine::sim::generate::{self, EVENTS};
use zirv_flow_engine::sim::rng::Rng;
use zirv_flow_store::{
    EnrollJobPayload, EnrollWork, ManualClock, TriggerKind, create_signing_key, occurrence_of,
};

use super::super::flow_step::tests::Env;
use super::mock::Mock;

/// Virtual time starts here, the same instant as the engine harness.
pub(super) const EPOCH: OffsetDateTime = datetime!(2030-01-01 0:00 UTC);
/// The days the run lasts.
pub(super) const DAYS: i64 = 90;
/// People are enrolled over this many days from the epoch.
const ENROLL_DAYS: u64 = 20;
/// The traits the generated `action.update_trait` nodes write, `t0` to `t4`.
pub(super) const TRAIT_KEYS: [&str; 5] = ["t0", "t1", "t2", "t3", "t4"];
const PLANS: [&str; 4] = ["free", "pro", "team", "trial"];

/// One person of the population.
pub(super) struct Person {
    pub(super) id: Uuid,
    pub(super) email: String,
    pub(super) flow: usize,
}

/// One published flow and the graph it was generated as.
pub(super) struct SimFlow {
    pub(super) id: Uuid,
    pub(super) graph: FlowGraph,
}

pub(super) struct World {
    pub(super) env: Env,
    pub(super) seed: u64,
    pub(super) mock: Mock,
    pub(super) keyring: Arc<Keyring>,
    pub(super) egress: Arc<EgressPolicy>,
    pub(super) people: Vec<Person>,
    pub(super) flows: Vec<SimFlow>,
    /// The four lists the generated `action.list` nodes use, in this workspace.
    pub(super) lists: Vec<Uuid>,
}

/// The instant the run stops at.
pub(super) fn end() -> OffsetDateTime {
    EPOCH + Span::days(DAYS)
}

impl World {
    /// Builds the world for `seed` with `population` people, in a workspace of its own named
    /// after `ordinal`, so several seeds can share one database.
    pub(super) async fn build(pool: PgPool, ordinal: usize, seed: u64, population: usize) -> Self {
        zirv_flow_store::migrate(&pool).await.unwrap();
        install_enqueue_log(&pool).await;
        install_write_log(&pool).await;
        let slug = format!("sim-{ordinal}");
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse(&slug).unwrap(),
            &Slug::parse(&slug).unwrap(),
        )
        .await
        .unwrap()
        .id;
        let env = Env {
            pool,
            ws,
            clock: ManualClock::new(EPOCH),
        };
        let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let mock = Mock::new(seed);
        let port = mock.serve().await;
        let mut world = Self {
            env,
            seed,
            mock,
            keyring,
            egress: Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap()),
            people: Vec::new(),
            flows: Vec::new(),
            lists: (0..4_u128)
                .map(|index| {
                    Uuid::from_u128(generate::list_id(index).as_u128() + (ordinal as u128) * 16)
                })
                .collect(),
        };
        world.define().await;
        world.sign().await;
        world.publish_flows(port, population).await;
        world.add_people(population).await;
        world.schedule_enrollments().await;
        world
    }

    /// The definitions the generated flows rely on: the traits they write and branch on, the
    /// events they wait for and four lists under the ids the generator names.
    async fn define(&self) {
        let env = &self.env;
        env.define_trait("plan", TraitType::String).await;
        for key in TRAIT_KEYS {
            env.define_trait(key, TraitType::Number).await;
        }
        for name in EVENTS {
            env.define_event(name).await;
        }
        for (index, list) in self.lists.iter().enumerate() {
            sqlx::query("INSERT INTO lists (id, workspace_id, name) VALUES ($1, $2, $3)")
                .bind(list)
                .bind(env.ws.as_uuid())
                .bind(format!("List {index}"))
                .execute(&env.pool)
                .await
                .unwrap();
        }
    }

    /// A signing secret, so webhook calls are sent and not refused.
    async fn sign(&self) {
        let env = &self.env;
        let secret = generate_secret();
        let id = SecretId::new();
        let sealed = seal(
            &self.keyring,
            secret.expose().as_bytes(),
            &secret_aad(env.ws, id, SecretPurpose::WebhookSigning),
        );
        zirv_flow_store::secrets::insert_unchecked(
            &env.pool,
            env.ws,
            SecretPurpose::WebhookSigning,
            id,
            &sealed,
        )
        .await
        .unwrap();
        let mut connection = env.pool.acquire().await.unwrap();
        assert!(
            create_signing_key(&mut connection, env.ws, id, env.clock.now())
                .await
                .unwrap()
        );
    }

    /// Generates and publishes the flows. Every other one holds every node kind. The webhook
    /// nodes point at the mock destination.
    async fn publish_flows(&mut self, port: u16, population: usize) {
        let count = (population / 20).clamp(4, 12);
        let mut rng = Rng::derive(self.seed, &[2]);
        let mut lists = Rng::derive(self.seed, &[5]);
        let mut waits = Rng::derive(self.seed, &[6]);
        for index in 0..count {
            let size = usize::try_from(rng.range(10, 30)).unwrap_or(12);
            let mut graph = generate::graph(&mut rng, size, index % 2 == 0);
            for node in &mut graph.nodes {
                match &mut node.kind {
                    NodeKind::ActionWebhook { url, .. } => {
                        *url = format!("http://127.0.0.1:{port}/hook");
                    }
                    // The generator names four fixed lists and mostly adds; each workspace has
                    // its own lists. Every list node here uses the first one and adds or removes
                    // by the seed, so that a person's walk adds to a list it is in already and
                    // removes from one it joined, which the write log has to count.
                    NodeKind::ActionList { list_id, change } => {
                        *list_id = self.lists[0];
                        *change = if lists.chance(55) {
                            ListChange::Add
                        } else {
                            ListChange::Remove
                        };
                    }
                    _ => {}
                }
            }
            settle_splits(&mut graph);
            wait_for_outcomes(&mut graph, &mut waits);
            let id = self
                .env
                .flow(
                    &format!("Sim {index}"),
                    serde_json::to_value(&graph).unwrap(),
                )
                .await;
            self.flows.push(SimFlow { id, graph });
        }
    }

    async fn add_people(&mut self, population: usize) {
        let mut rng = Rng::derive(self.seed, &[3]);
        for index in 0..population {
            let email = format!("person{index}@sim.example.com");
            let plan = *rng.pick(&PLANS);
            let id = self.env.person(&email, json!({"plan": plan})).await;
            let flow = rng.index(self.flows.len());
            self.people.push(Person { id, email, flow });
        }
    }

    /// Queues `flow.enroll` jobs for groups of people of one flow, each due at a seeded instant
    /// of the first days.
    async fn schedule_enrollments(&self) {
        let mut rng = Rng::derive(self.seed, &[4]);
        for (flow_index, flow) in self.flows.iter().enumerate() {
            let members: Vec<usize> = (0..self.people.len())
                .filter(|index| self.people[*index].flow == flow_index)
                .collect();
            let mut rest = members.as_slice();
            let mut batch = 0_u64;
            while !rest.is_empty() {
                let take = usize::try_from(rng.range(1, 12))
                    .unwrap_or(1)
                    .min(rest.len());
                let (group, tail) = rest.split_at(take);
                rest = tail;
                let pairs = group
                    .iter()
                    .map(|index| {
                        let occurrence = occurrence_of(&format!("sim:{}:{index}", self.seed));
                        (self.people[*index].id, occurrence)
                    })
                    .collect();
                let payload = EnrollJobPayload {
                    workspace: self.env.ws,
                    flow: flow.id,
                    version: Some(1),
                    trigger: TriggerKind::Manual,
                    work: EnrollWork::People(pairs),
                };
                let trace = TraceContext::root(Cause::api("sim"));
                let mut job = payload.job(&trace, format!("sim:enroll:{flow_index}:{batch}"));
                let seconds = i64::try_from(rng.below(ENROLL_DAYS * 86_400)).unwrap_or(0);
                job.run_at = Some(EPOCH + Span::seconds(seconds));
                zirv_flow_store::enqueue(&self.env.pool, &job)
                    .await
                    .unwrap();
                batch += 1;
            }
        }
    }
}

/// A trigger on the test database's `jobs` table that logs every `webhook.call` job that is
/// inserted and committed: the queue forgets a job once it completes, so only a log can tell
/// that an external effect was enqueued twice, the second time after the first call finished.
/// It is a test fixture; the product schema is untouched.
async fn install_enqueue_log(pool: &PgPool) {
    for statement in [
        "CREATE TABLE IF NOT EXISTS sim_enqueued ( \
             n bigserial PRIMARY KEY, workspace_id uuid, kind text NOT NULL, \
             unique_key text, payload jsonb NOT NULL)",
        "CREATE OR REPLACE FUNCTION sim_log_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
             INSERT INTO sim_enqueued (workspace_id, kind, unique_key, payload) \
             VALUES (NEW.workspace_id, NEW.kind, NEW.unique_key, NEW.payload); \
             RETURN NEW; \
         END $$",
        "DROP TRIGGER IF EXISTS sim_log_enqueue ON jobs",
        "CREATE TRIGGER sim_log_enqueue AFTER INSERT ON jobs FOR EACH ROW \
         WHEN (NEW.kind IN ('webhook.call', 'delivery.send')) EXECUTE FUNCTION sim_log_enqueue()",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

/// Triggers on the tables the internal effects write, logging every write attempt, one row per
/// attempt: a flow's contribution to a trait (a `source_records` row whose source is
/// `flow:<id>`) and list membership. A row trigger runs before the statement acts on the row,
/// so it also fires for an `INSERT ... ON CONFLICT DO NOTHING` row that conflicts: a repeated
/// list add is logged although it changes nothing. The log lives in the transaction that wrote
/// it, so an attempt rolled back disappears, and what remains is every committed application.
///
/// Two repeats leave no row to fire on, and are therefore not seen: a `DELETE` that finds no
/// row (removing a non-member) and a trait write whose content hash equals the ledger row's
/// (the same value at the same instant, which the store skips before writing). The checks count
/// the attempts they predict with that in mind (see `invariants`). Test fixture only.
async fn install_write_log(pool: &PgPool) {
    for statement in [
        "CREATE TABLE IF NOT EXISTS sim_writes ( \
             n bigserial PRIMARY KEY, kind text NOT NULL, workspace_id uuid NOT NULL, \
             person_id uuid NOT NULL, target text NOT NULL, flow uuid, op text NOT NULL)",
        "CREATE OR REPLACE FUNCTION sim_log_trait_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
             IF TG_OP = 'DELETE' THEN \
                 INSERT INTO sim_writes (kind, workspace_id, person_id, target, flow, op) \
                 VALUES ('trait', OLD.workspace_id, OLD.person_id, 'traits', \
                         substr(OLD.source, 6)::uuid, TG_OP); \
                 RETURN OLD; \
             END IF; \
             INSERT INTO sim_writes (kind, workspace_id, person_id, target, flow, op) \
             VALUES ('trait', NEW.workspace_id, NEW.person_id, 'traits', \
                     substr(NEW.source, 6)::uuid, TG_OP); \
             RETURN NEW; \
         END $$",
        "DROP TRIGGER IF EXISTS sim_log_trait_write ON source_records",
        "CREATE TRIGGER sim_log_trait_write BEFORE INSERT OR UPDATE ON source_records \
         FOR EACH ROW WHEN (NEW.source LIKE 'flow:%') EXECUTE FUNCTION sim_log_trait_write()",
        "DROP TRIGGER IF EXISTS sim_log_trait_delete ON source_records",
        "CREATE TRIGGER sim_log_trait_delete BEFORE DELETE ON source_records \
         FOR EACH ROW WHEN (OLD.source LIKE 'flow:%') EXECUTE FUNCTION sim_log_trait_write()",
        "CREATE OR REPLACE FUNCTION sim_log_list_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
             IF TG_OP = 'DELETE' THEN \
                 INSERT INTO sim_writes (kind, workspace_id, person_id, target, op) \
                 VALUES ('list', OLD.workspace_id, OLD.person_id, OLD.list_id::text, TG_OP); \
                 RETURN OLD; \
             END IF; \
             INSERT INTO sim_writes (kind, workspace_id, person_id, target, op) \
             VALUES ('list', NEW.workspace_id, NEW.person_id, NEW.list_id::text, TG_OP); \
             RETURN NEW; \
         END $$",
        "DROP TRIGGER IF EXISTS sim_log_list_insert ON list_members",
        "CREATE TRIGGER sim_log_list_insert BEFORE INSERT ON list_members \
         FOR EACH ROW WHEN (NEW.source = 'flow') EXECUTE FUNCTION sim_log_list_write()",
        "DROP TRIGGER IF EXISTS sim_log_list_delete ON list_members",
        "CREATE TRIGGER sim_log_list_delete BEFORE DELETE ON list_members \
         FOR EACH ROW EXECUTE FUNCTION sim_log_list_write()",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

/// Puts a wait for the outcome of the call after most webhook nodes: the node's `next` edge now
/// leads to a `wait.until_event` on `webhook.succeeded` or `webhook.failed`, whose two ports
/// lead on to what the edge led to. The runtime writes those events itself, once per committed
/// execution of the call, and a call that commits without its ack writes one twice; a wait that
/// settles on the first has to ignore the second. The generator's own waits cannot do that: they
/// rarely follow a webhook node closely enough to be registered when the outcome arrives.
fn wait_for_outcomes(graph: &mut FlowGraph, rng: &mut Rng) {
    let hooks: Vec<String> = graph
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::ActionWebhook { .. }))
        .map(|node| node.id.clone())
        .collect();
    for (count, hook) in hooks.iter().enumerate() {
        let Some(edge) = graph
            .edges
            .iter_mut()
            .find(|edge| edge.from == *hook && edge.port == PORT_NEXT)
        else {
            continue;
        };
        let wait = format!("w{count}");
        let target = std::mem::replace(&mut edge.to, wait.clone());
        let name = if rng.chance(70) {
            WEBHOOK_SUCCEEDED_EVENT
        } else {
            WEBHOOK_FAILED_EVENT
        };
        graph.nodes.push(FlowNode {
            id: wait.clone(),
            kind: NodeKind::WaitUntilEvent {
                event: EventMatch {
                    name: name.to_owned(),
                    filters: Vec::new(),
                },
                timeout_seconds: rng.range(3_600, 3 * 86_400),
            },
        });
        for (suffix, port) in [("event", PORT_EVENT), ("timeout", PORT_TIMEOUT)] {
            graph.edges.push(Edge {
                id: format!("ew{count}{suffix}"),
                from: wait.clone(),
                port: port.to_owned(),
                to: target.clone(),
            });
        }
    }
    let issues = validate_graph(graph);
    assert!(
        issues.is_empty(),
        "the outcome waits made an invalid graph: {issues:?}"
    );
}

/// Makes every arm of a `split.random` lead to the same node. The engine picks the arm from a
/// hash of the enrollment's id, which the runtime draws at random, so the path a person takes
/// would differ from run to run and the run could not follow its seed. The node still runs, and
/// the arm it picks still shows in the decisions; only where it leads no longer depends on it.
fn settle_splits(graph: &mut FlowGraph) {
    let splits: Vec<String> = graph
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::SplitRandom { .. }))
        .map(|node| node.id.clone())
        .collect();
    for split in splits {
        let Some(target) = graph
            .edges
            .iter()
            .find(|edge| edge.from == split)
            .map(|edge| edge.to.clone())
        else {
            continue;
        };
        for edge in graph.edges.iter_mut().filter(|edge| edge.from == split) {
            edge.to.clone_from(&target);
        }
    }
    // What only another arm led to is out of reach now, and a node nothing leads to would be a
    // second entry: drop what the first node (the entry) no longer reaches.
    let mut reached: Vec<String> = graph.nodes.iter().take(1).map(|n| n.id.clone()).collect();
    let mut index = 0;
    while index < reached.len() {
        let from = reached[index].clone();
        for edge in graph.edges.iter().filter(|edge| edge.from == from) {
            if !reached.contains(&edge.to) {
                reached.push(edge.to.clone());
            }
        }
        index += 1;
    }
    graph.nodes.retain(|node| reached.contains(&node.id));
    graph.edges.retain(|edge| reached.contains(&edge.from));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(seed: u64, cover_all: bool) -> FlowGraph {
        generate::graph(&mut Rng::derive(seed, &[2]), 24, cover_all)
    }

    fn settled(seed: u64) -> FlowGraph {
        let mut graph = generated(seed, true);
        settle_splits(&mut graph);
        wait_for_outcomes(&mut graph, &mut Rng::derive(seed, &[6]));
        graph
    }

    #[test]
    fn the_same_seed_builds_the_same_flows() {
        for seed in 0..20 {
            assert_eq!(settled(seed), settled(seed));
        }
        assert_ne!(settled(1), settled(2));
    }

    #[test]
    fn every_arm_of_a_split_leads_to_one_node_and_the_graph_stays_valid() {
        let mut splits = 0;
        for seed in 0..60 {
            let graph = settled(seed);
            assert!(validate_graph(&graph).is_empty(), "seed {seed}");
            for node in &graph.nodes {
                if matches!(node.kind, NodeKind::SplitRandom { .. }) {
                    splits += 1;
                    let targets: Vec<&str> = graph
                        .edges
                        .iter()
                        .filter(|edge| edge.from == node.id)
                        .map(|edge| edge.to.as_str())
                        .collect();
                    assert!(targets.len() >= 2);
                    assert!(targets.iter().all(|to| *to == targets[0]), "{targets:?}");
                }
            }
        }
        assert!(splits > 0, "no generated graph held a split");
    }

    #[test]
    fn every_webhook_node_is_followed_by_a_wait_for_its_outcome() {
        let mut hooks = 0;
        for seed in 0..60 {
            let graph = settled(seed);
            for node in &graph.nodes {
                if !matches!(node.kind, NodeKind::ActionWebhook { .. }) {
                    continue;
                }
                hooks += 1;
                let next = graph
                    .edges
                    .iter()
                    .find(|edge| edge.from == node.id && edge.port == PORT_NEXT)
                    .and_then(|edge| graph.node(&edge.to))
                    .expect("a webhook node leads on");
                let NodeKind::WaitUntilEvent { event, .. } = &next.kind else {
                    panic!("seed {seed}: {} is not followed by a wait", node.id);
                };
                assert!(
                    [WEBHOOK_SUCCEEDED_EVENT, WEBHOOK_FAILED_EVENT].contains(&event.name.as_str())
                );
            }
        }
        assert!(hooks > 0, "no generated graph held a webhook node");
    }
}
