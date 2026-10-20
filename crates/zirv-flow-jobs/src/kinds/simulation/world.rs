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
use zirv_flow_core::flow::{FlowGraph, NodeKind};
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
        let mock = Mock::default();
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
        for index in 0..count {
            let size = usize::try_from(rng.range(10, 30)).unwrap_or(12);
            let mut graph = generate::graph(&mut rng, size, index % 2 == 0);
            for node in &mut graph.nodes {
                match &mut node.kind {
                    NodeKind::ActionWebhook { url, .. } => {
                        *url = format!("http://127.0.0.1:{port}/hook");
                    }
                    // The generator names four fixed lists; each workspace has its own.
                    NodeKind::ActionList { list_id, .. } => {
                        let slot = list_id.as_u128() - generate::list_id(0).as_u128();
                        *list_id = self.lists[usize::try_from(slot).unwrap_or(0)];
                    }
                    _ => {}
                }
            }
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
         WHEN (NEW.kind = 'webhook.call') EXECUTE FUNCTION sim_log_enqueue()",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}
