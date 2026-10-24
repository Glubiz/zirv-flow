//! `delivery.sync`: one profile operation of an `action.sync` node, delivered through a provider
//! destination (architecture 10.1 and 10.2, "As built (profile sync, #226)").
//!
//! `flow.step` enqueues the job in the step's transaction under the node's effect key, so the
//! operation is queued exactly when the step commits and at most once per step. The job follows
//! the pattern of `webhook.call` (`kinds/webhook_call.rs`):
//!
//! - It reads the person, their external key and the listed traits with no lock, then calls the
//!   destination, and locks the person (`FOR KEY SHARE`) only to write the outcome.
//! - The call goes through the shared delivery path (`crate::delivery`): the effect key is the
//!   idempotency key, the destination's rate bucket admits it, the verdict is the one every
//!   outbound job uses, and the attempt is recorded in `delivery_attempts`.
//! - A 2xx succeeds; a 408, 429, 5xx, timeout or transport error is retried with the worker's
//!   backoff (or the `Retry-After`, which also slows the bucket); anything else fails at once.
//!   The last attempt of a retried failure is recorded as a failure instead of dying unseen.
//! - Succeeding and failing both end the job as done and write `sync.succeeded` or `sync.failed`
//!   with a decision (`sync.upsert` or `sync.remove`), through the writer `webhook.call` uses.
//!
//! A person who is gone, exited or merged away gets no call and no outcome. A person without the
//! external key gets no call and `sync.failed` with the reason `missing_key`. A wait for a rate
//! token re-enqueues the job (six hours at the most) and spends no attempt.
//!
//! Neither the external key nor a trait value reaches an event, a decision, a log line or a
//! metric: they are in the outbound request only.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    DestinationContext, ProfileCall, Registry, provider_profile_key_source,
};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::manifest::KeySource;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::flow::SyncOperation;
use zirv_flow_store::{
    Admission, Destination, NewJob, PgPool, SYNC_JOB_KIND, SubjectKind, SyncDetail, WebhookOutcome,
    WebhookPerson, WebhookResult, WebhookSubject, find_destination, list_identities,
    lock_webhook_person, read_webhook_subject, record_webhook_outcome, throttle_bucket,
};

use crate::delivery::attempt::AttemptTarget;
use crate::delivery::{self, Attempt, Verdict, credential};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct DeliverySync;

/// The most a sync waits for a rate token before it ends as `rate_limited`.
const MAX_RATE_WAIT: time::Duration = time::Duration::hours(6);

/// The payload `flow.step` enqueues.
struct SyncJob {
    workspace: WorkspaceId,
    node: String,
    enrollment: Uuid,
    /// The node's effect key: the idempotency key at the destination on every attempt.
    idempotency_key: String,
    /// The subject of the `delivery_attempts` rows; minted with the job and kept across waits.
    sync_id: Uuid,
    destination: Uuid,
    operation: SyncOperation,
    trait_keys: Vec<String>,
    /// How many calls earlier runs of this sync made (a wait for a token re-enqueues the job).
    attempt_base: i32,
    /// When the sync first waited for a rate token.
    first_wait: Option<OffsetDateTime>,
    deferrals: u32,
}

impl SyncJob {
    fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let id = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        let operation = match text("operation")? {
            "upsert" => SyncOperation::Upsert,
            "remove" => SyncOperation::Remove,
            _ => return None,
        };
        let first_wait = match payload.get("first_wait") {
            None | Some(Value::Null) => None,
            Some(value) => Some(OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()?),
        };
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            node: text("node_id")?.to_owned(),
            enrollment: id("enrollment_id")?,
            idempotency_key: text("idempotency_key")?.to_owned(),
            sync_id: id("sync_id")?,
            destination: id("destination_id")?,
            operation,
            trait_keys: payload
                .get("trait_keys")?
                .as_array()?
                .iter()
                .map(|key| key.as_str().map(str::to_owned))
                .collect::<Option<_>>()?,
            attempt_base: payload
                .get("attempt_base")
                .and_then(Value::as_i64)
                .and_then(|base| i32::try_from(base).ok())
                .unwrap_or(0),
            first_wait,
            deferrals: payload
                .get("deferrals")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0),
        })
    }
}

/// What the destination and its manifest give the job, or the fixed reason the sync cannot go.
struct Plan {
    destination: Destination,
    driver: &'static dyn zirv_flow_connectors::destinations::ErasedDestination,
    source: KeySource,
}

fn plan(found: Option<Destination>, operation: SyncOperation) -> Result<Plan, &'static str> {
    let destination = found
        .filter(|destination| destination.archived_at.is_none())
        .ok_or("destination_unavailable")?;
    let registry = Registry::built_in();
    let driver = registry
        .get(&destination.kind)
        .filter(|driver| {
            driver
                .spec()
                .capabilities
                .channels
                .contains(&"profile_sync")
        })
        .ok_or("destination_unavailable")?;
    // The manifest declares the operation exactly when it names where the key comes from.
    let source = provider_profile_key_source(&destination.config, operation)
        .ok_or("operation_unsupported")?;
    Ok(Plan {
        destination,
        driver,
        source,
    })
}

/// A trait value as an external key: a non-empty string or a number.
fn key_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

impl JobKind for DeliverySync {
    const KIND: &'static str = SYNC_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = SyncJob::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a profile sync"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let attempts = ctx.attempts();
        let number = job.attempt_base + attempts;

        let found = find_destination(ctx.conn(), job.workspace, job.destination).await?;
        let plan = plan(found, job.operation);
        // The traits to send, and the one the key is read from when it is a trait. Read with no
        // lock: the call below can take long, and nothing an ingest batch or an erasure needs
        // may be held for that long.
        let mut keys = job.trait_keys.clone();
        if let Ok(Plan {
            source: KeySource::Trait(key),
            ..
        }) = &plan
            && !keys.contains(key)
        {
            keys.push(key.clone());
        }
        let Some(WebhookSubject { person, traits }) =
            read_webhook_subject(ctx.conn(), job.workspace, job.enrollment, &keys).await?
        else {
            // Erased with the person, exited or merged away: there is nobody to sync, and
            // nothing to retry.
            return Ok(());
        };

        let done = match plan {
            Err(reason) => ended(reason),
            Ok(plan) => {
                let key = match &plan.source {
                    KeySource::Identity(kind) => list_identities(ctx.conn(), job.workspace, person)
                        .await?
                        .into_iter()
                        .find(|identity| &identity.kind == kind)
                        .map(|identity| identity.value)
                        .filter(|value| !value.trim().is_empty()),
                    KeySource::Trait(key) => traits.get(key).and_then(key_text),
                };
                match key {
                    None => ended("missing_key"),
                    Some(key) => {
                        let listed: Map<String, Value> = traits
                            .into_iter()
                            .filter(|(name, _)| job.trait_keys.contains(name))
                            .collect();
                        match call(ctx, &job, &plan, (person, &key), &listed, number, now).await? {
                            Called::Done(done) => done,
                            Called::Waiting => return Ok(()),
                        }
                    }
                }
            }
        };

        let last_attempt = attempts >= ctx.max_attempts();
        let (result, status, reason) = match done.verdict {
            Verdict::Succeeded { status } => (WebhookResult::Succeeded, Some(status), None),
            Verdict::Terminal { status, reason } => (WebhookResult::Failed, status, Some(reason)),
            Verdict::Retry {
                status,
                reason,
                message,
                retry_after,
            } => {
                warn!(
                    status,
                    attempt = number,
                    reason,
                    "profile sync will be retried"
                );
                if !last_attempt {
                    let error = JobError::new(message);
                    return Err(retry_after.map_or(error, |wait| error.with_retry_after(wait)));
                }
                (WebhookResult::Failed, status, Some(reason))
            }
        };
        if result == WebhookResult::Failed {
            warn!(
                destination = %job.destination,
                attempt = number,
                reason,
                "profile sync failed"
            );
        }
        // The person is locked only now, for the short outcome write, and the enrollment checked
        // again: an erasure that committed during the call leaves no event and no decision.
        match lock_webhook_person(ctx.conn(), job.workspace, job.enrollment, person).await? {
            WebhookPerson::Held => {}
            WebhookPerson::Gone | WebhookPerson::Moved => return Ok(()),
        }
        let trace = ctx.trace().clone();
        let job_ref = (ctx.job_id(), attempts);
        record_webhook_outcome(
            ctx.conn(),
            &WebhookOutcome {
                workspace: job.workspace,
                enrollment: job.enrollment,
                person,
                node: &job.node,
                result,
                status,
                attempts: number,
                reason,
                trace: &trace,
                now,
                job: job_ref,
                sync: Some(SyncDetail {
                    destination: job.destination,
                    operation: job.operation,
                }),
            },
        )
        .await?;
        Ok(())
    }
}

/// A sync that ends here, with no call, for a fixed reason.
fn ended(reason: &'static str) -> Attempt {
    Attempt {
        verdict: Verdict::Terminal {
            status: None,
            reason,
        },
        elapsed: Duration::ZERO,
    }
}

enum Called {
    /// The destination was called, or the call could not be made; this is how it went.
    Done(Attempt),
    /// The destination's rate bucket is empty: the job was re-enqueued for later.
    Waiting,
}

/// Takes a rate token, opens the credential and calls the destination, then records the attempt.
async fn call(
    ctx: &mut JobContext<'_>,
    job: &SyncJob,
    plan: &Plan,
    (person, key): (Uuid, &str),
    traits: &Map<String, Value>,
    number: i32,
    now: OffsetDateTime,
) -> Result<Called, JobError> {
    let pool = ctx
        .pool()
        .cloned()
        .ok_or_else(|| JobError::new("the worker has no pool"))?;
    if job
        .first_wait
        .is_some_and(|first| now - first > MAX_RATE_WAIT)
    {
        return Ok(Called::Done(ended("rate_limited")));
    }
    let gates = Arc::clone(ctx.rate_gates());
    let clock = ctx.clock().clone();
    match gates.admit(&pool, plan.destination.id, &clock).await? {
        Admission::Granted => {}
        Admission::Wait(wait) => {
            // Whether the copy was enqueued or the person is gone, this run ends with no call.
            wait_for_token(ctx, job, person, wait, now).await?;
            return Ok(Called::Waiting);
        }
        Admission::Gone => return Ok(Called::Done(ended("destination_unavailable"))),
    }

    let credential =
        match credential::open(ctx, job.workspace, plan.destination.credential_id).await? {
            Ok(credential) => credential,
            Err(verdict) => {
                return Ok(Called::Done(Attempt {
                    verdict,
                    elapsed: Duration::ZERO,
                }));
            }
        };
    // No redirects: a redirect would turn the call into something that was never the operation.
    let limits = HttpLimits {
        max_redirects: 0,
        ..HttpLimits::default()
    };
    let context = DestinationContext::new(
        ctx.egress().clone(),
        limits,
        plan.driver.spec().kind,
        plan.destination.id.to_string(),
    )
    .map_err(|_| JobError::new("the outbound client could not be built"))?;
    let profile = ProfileCall {
        operation: job.operation,
        key,
        traits,
        idempotency_key: &job.idempotency_key,
    };
    let done = delivery::sync(
        plan.driver,
        &context,
        &plan.destination.config,
        &credential,
        &profile,
        now,
    )
    .await;
    let target = AttemptTarget {
        destination_id: plan.destination.id,
        subject_kind: SubjectKind::Sync,
        subject_id: job.sync_id,
    };
    delivery::settle(ctx, Some(&target), number, &done).await?;
    if let Verdict::Retry {
        reason: "http_429",
        retry_after,
        ..
    } = done.verdict
    {
        slow_down(&pool, &plan.destination, retry_after, &clock).await;
    }
    Ok(Called::Done(done))
}

/// A destination answered 429: halve its bucket's rate until its `Retry-After` has passed. A
/// failure to record it is logged and does not change the job's result.
async fn slow_down(
    pool: &PgPool,
    destination: &Destination,
    retry_after: Option<Duration>,
    clock: &zirv_flow_store::Clock,
) {
    if throttle_bucket(pool, destination.id, retry_after, clock)
        .await
        .is_err()
    {
        warn!(destination = %destination.id, "a destination's rate could not be slowed");
    }
}

/// Re-enqueues the sync for when the bucket holds a token. The wait spends no attempt: the next
/// run has the attempts this one had left, and numbers its calls on from the last one made.
///
/// The copy names the person, and an erasure purges the jobs that do. So the person is locked
/// (`FOR KEY SHARE`, which the erasure's delete conflicts with) before the copy is inserted, and
/// nothing is enqueued when the person is gone, exited or merged away. An erasure that holds the
/// person first makes this wait, then find them gone; one that comes later waits for this
/// transaction, and its purge, which runs after the delete, sees the committed copy.
async fn wait_for_token(
    ctx: &mut JobContext<'_>,
    job: &SyncJob,
    person: Uuid,
    wait: Duration,
    now: OffsetDateTime,
) -> Result<(), JobError> {
    match lock_webhook_person(ctx.conn(), job.workspace, job.enrollment, person).await? {
        WebhookPerson::Held => {}
        WebhookPerson::Gone | WebhookPerson::Moved => return Ok(()),
    }
    let attempts = ctx.attempts();
    let mut payload = ctx.payload().clone();
    if let Some(map) = payload.as_object_mut() {
        let first = job.first_wait.unwrap_or(now);
        map.insert(
            "first_wait".to_owned(),
            Value::from(
                first
                    .format(&Rfc3339)
                    .map_err(|_| JobError::new("the time cannot be formatted"))?,
            ),
        );
        map.insert("deferrals".to_owned(), Value::from(job.deferrals + 1));
        map.insert(
            "attempt_base".to_owned(),
            Value::from(job.attempt_base + attempts - 1),
        );
    }
    let mut next = NewJob::new(SYNC_JOB_KIND, ctx.trace().clone());
    next.workspace_id = Some(job.workspace);
    next.payload = payload;
    next.run_at = Some(now + wait);
    next.max_attempts = (ctx.max_attempts() - (attempts - 1)).max(1);
    next.unique_key = Some(format!(
        "{}:wait:{}",
        job.idempotency_key,
        job.deferrals + 1
    ));
    ctx.enqueue(&next).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_connectors::destinations::{
        FakeProvider, FakeProviderOptions, FakeProviderScript,
    };
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{
        ClaimedJob, EnrollRequest, Enrolled, NewFlow, NewTraitDefinition, Occurrence, ProfileRecord,
    };

    use super::*;
    use crate::context::IngestSettings;

    const FIXTURE: &str =
        include_str!("../../../zirv-flow-connectors/tests/fixtures/providers/profile_sync.json");
    const EMAIL: &str = "ada@example.com";
    const TRAIT_VALUE: &str = "SENTINEL-TRAIT-VALUE";

    /// One workspace with a provider destination on a fake provider, a person and an enrollment.
    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        fake: FakeProvider,
        destination: Uuid,
    }

    impl Env {
        async fn new(pool: PgPool) -> Self {
            Self::with(pool, |manifest| manifest, 100, 100).await
        }

        async fn with(
            pool: PgPool,
            adjust: impl FnOnce(Value) -> Value,
            rate: i32,
            burst: i32,
        ) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse("ours").unwrap(),
                &Slug::parse("ours").unwrap(),
            )
            .await
            .unwrap()
            .id;
            for key in ["plan", "country", "age"] {
                zirv_flow_store::create_trait_definition(
                    &pool,
                    ws,
                    &NewTraitDefinition {
                        key: key.into(),
                        trait_type: TraitType::String,
                        precedence: Precedence::Latest,
                        source_order: Vec::new(),
                        pii: false,
                        channel_address: false,
                        inferred: false,
                        description: String::new(),
                    },
                )
                .await
                .unwrap();
            }
            let fake = FakeProvider::start(FakeProviderOptions::default()).await;
            let mut manifest: Value = serde_json::from_str(FIXTURE).unwrap();
            manifest["base_url"] = json!(fake.base_url());
            manifest["auth"] = json!({"type": "none"});
            let config = json!({"manifest": adjust(manifest)});
            let destination = Uuid::now_v7();
            zirv_flow_store::create_destination(
                &pool,
                ws,
                &zirv_flow_store::NewDestination {
                    id: destination,
                    kind: "provider",
                    name: "profiles",
                    config: &config,
                    credential_id: None,
                    rate,
                    burst,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            Self {
                pool,
                ws,
                fake,
                destination,
            }
        }

        async fn person(&self) -> Uuid {
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: EMAIL.into(),
                identifiers: vec![("email".into(), EMAIL.into())],
                traits: json!({"plan": TRAIT_VALUE, "age": "41"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                observed_at: time::OffsetDateTime::now_utc() - time::Duration::days(1),
                trace: TraceContext::root(Cause::api("t")),
            };
            match &zirv_flow_store::apply_profile_batch(&self.pool, self.ws, &[record])
                .await
                .unwrap()[0]
            {
                zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
                | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => *person_id,
                zirv_flow_store::ProfileOutcome::Rejected { reason } => {
                    panic!("rejected: {reason}")
                }
            }
        }

        async fn enroll(&self, person: Uuid) -> Enrolled {
            let id = Uuid::now_v7();
            let graph = json!({"v": 1, "nodes": [
                {"id": "push", "type": "action.sync", "destination": self.destination,
                    "operation": "upsert", "trait_keys": ["plan", "country"]},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "push", "port": "next", "to": "end"}]});
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id,
                    name: format!("Sync {id}"),
                    graph,
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, id)
                .await
                .unwrap();
            let (_, version) = zirv_flow_store::publish_flow(
                &mut tx,
                &zirv_flow_store::Clock::default(),
                self.ws,
                &locked,
                None,
                &TraceContext::root(Cause::api("publish")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let outcome = zirv_flow_store::enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: self.ws,
                    flow: id,
                    version: version.version,
                    people: &[person],
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: time::OffsetDateTime::now_utc(),
                    trace: &TraceContext::root(Cause::api("enroll")),
                },
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            outcome.enrolled[0].clone()
        }

        /// Queues the job `flow.step` would.
        async fn queue(&self, enrolled: &Enrolled, operation: &str, max_attempts: i32) -> String {
            let key = format!("{}:push:sync:{operation}", enrolled.enrollment);
            let mut job = NewJob::new(SYNC_JOB_KIND, enrolled.trace.clone());
            job.workspace_id = Some(self.ws);
            job.unique_key = Some(key.clone());
            job.max_attempts = max_attempts;
            let trait_keys = if operation == "upsert" {
                json!(["plan", "country"])
            } else {
                json!([])
            };
            job.payload = json!({
                "workspace_id": self.ws,
                "flow_id": Uuid::from_u128(9),
                "version": 1,
                "node_id": "push",
                "enrollment_id": enrolled.enrollment,
                "person_id": Uuid::from_u128(3),
                "trace_id": enrolled.trace.trace_id,
                "idempotency_key": key,
                "sync_id": Uuid::now_v7(),
                "destination_id": self.destination,
                "operation": operation,
                "trait_keys": trait_keys,
            });
            zirv_flow_store::enqueue(&self.pool, &job).await.unwrap();
            key
        }

        async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[SYNC_JOB_KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
        }

        /// The handler on `claimed`, in the worker's transaction `tx`.
        async fn run(
            &self,
            tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
            claimed: &ClaimedJob,
        ) -> Result<(), JobError> {
            let egress = Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap());
            let mut ctx = JobContext::new(tx, claimed)
                .with_outbound(None, egress, Arc::default())
                .with_ingest(IngestSettings {
                    pool: self.pool.clone(),
                    redaction_key: None,
                    batch_size: 1,
                    shards: 1,
                });
            DeliverySync.run(&mut ctx, claimed.payload.clone()).await
        }

        /// Claims the job and runs one attempt as a worker does: the work commits with the
        /// job's completion, and an error rolls it back and puts the job up again at once.
        async fn attempt(&self) -> Result<(), JobError> {
            let claimed = self.claim().await.expect("a delivery.sync job is ready");
            self.attempt_claimed(claimed).await
        }

        /// As [`attempt`](Self::attempt), for a job already claimed.
        async fn attempt_claimed(&self, claimed: ClaimedJob) -> Result<(), JobError> {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let result = self.run(&mut tx, &claimed).await;
            match result {
                Ok(()) => {
                    zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                        .await
                        .unwrap();
                    zirv_flow_store::commit(tx).await.unwrap();
                    Ok(())
                }
                Err(error) => {
                    drop(tx);
                    zirv_flow_store::fail_job(
                        &self.pool,
                        claimed.id,
                        claimed.lease_token,
                        Duration::ZERO,
                        error.message(),
                    )
                    .await
                    .unwrap();
                    Err(error)
                }
            }
        }

        async fn events(&self, name: &str) -> Vec<Value> {
            sqlx::query_scalar(
                "SELECT properties FROM events WHERE workspace_id = $1 AND name = $2 ORDER BY id",
            )
            .bind(self.ws.as_uuid())
            .bind(name)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// The (action, outcome, reason) of every decision of the sync family.
        async fn decisions(&self) -> Vec<(String, String, Option<String>)> {
            sqlx::query_as(
                "SELECT action, outcome, reason FROM decision_records \
                 WHERE workspace_id = $1 AND action LIKE 'sync.%' ORDER BY action",
            )
            .bind(self.ws.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn attempts(&self) -> Vec<(String, String, i32)> {
            sqlx::query_as(
                "SELECT subject_kind, outcome, attempt FROM delivery_attempts \
                 WHERE workspace_id = $1 ORDER BY attempt",
            )
            .bind(self.ws.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Everything the sync wrote that a person could read back: events and decisions.
        async fn written(&self) -> String {
            sqlx::query_scalar(
                "SELECT (SELECT coalesce(string_agg(properties::text, ' '), '') FROM events) || \
                        (SELECT coalesce(string_agg(subject_ids::text || coalesce(reason, ''), ' '), '') \
                         FROM decision_records)",
            )
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }
    }

    fn assert_no_personal_data(text: &str) {
        for needle in [EMAIL, "SENTINEL", "example.com"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upsert_sends_exactly_the_listed_traits_under_the_key_and_records_its_outcome(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        let key = env.queue(&enrolled, "upsert", 5).await;

        env.attempt().await.unwrap();

        let accepted = env.fake.accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].method, "PUT");
        assert_eq!(accepted[0].path, "/v1/profiles/ada%40example.com");
        assert_eq!(
            accepted[0].key, key,
            "the effect key is the idempotency key"
        );
        // `age` is on the person and not on the node: it is not sent. `country` is on the node
        // and not on the person: it is sent as null.
        assert_eq!(
            accepted[0].body,
            json!({
                "source": "zirv-flow",
                "profile": {
                    "id": EMAIL,
                    "attributes": {"plan": TRAIT_VALUE, "country": null}
                }
            })
        );
        let succeeded = env.events("sync.succeeded").await;
        assert_eq!(
            succeeded,
            [json!({
                "node_id": "push", "enrollment_id": enrolled.enrollment, "status": 200,
                "attempts": 1, "destination_id": env.destination, "operation": "upsert"
            })]
        );
        assert!(env.events("sync.failed").await.is_empty());
        assert_eq!(
            env.decisions().await,
            [("sync.upsert".to_owned(), "succeeded".to_owned(), None)]
        );
        assert_eq!(
            env.attempts().await,
            [("sync".to_owned(), "delivered".to_owned(), 1)]
        );
        assert_no_personal_data(&env.written().await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_remove_deletes_the_profile_under_the_same_key(pool: PgPool) {
        let env = Env::new(pool).await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        env.queue(&enrolled, "remove", 5).await;

        env.attempt().await.unwrap();

        let accepted = env.fake.accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].method, "DELETE");
        assert_eq!(accepted[0].path, "/v1/profiles/ada%40example.com");
        assert_eq!(accepted[0].body, Value::Null);
        assert_eq!(env.events("sync.succeeded").await.len(), 1);
        assert_eq!(env.decisions().await[0].0, "sync.remove");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_without_the_key_ends_missing_key_with_no_call_and_no_attempt(pool: PgPool) {
        let env = Env::with(
            pool,
            |mut manifest| {
                manifest["destination"]["upsert_profile"]["key_from"] =
                    json!({"identity": "phone"});
                manifest
            },
            100,
            100,
        )
        .await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        env.queue(&enrolled, "upsert", 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.fake.requests(), 0);
        let failed = env.events("sync.failed").await;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["status"], Value::Null);
        assert_eq!(
            env.decisions().await,
            [(
                "sync.upsert".to_owned(),
                "failed".to_owned(),
                Some("missing_key".to_owned())
            )]
        );
        assert!(env.attempts().await.is_empty());
        assert_no_personal_data(&env.written().await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_trait_can_be_the_key(pool: PgPool) {
        let env = Env::with(
            pool,
            |mut manifest| {
                manifest["destination"]["upsert_profile"]["key_from"] = json!({"trait": "age"});
                manifest
            },
            100,
            100,
        )
        .await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        env.queue(&enrolled, "upsert", 5).await;

        env.attempt().await.unwrap();

        let accepted = env.fake.accepted();
        assert_eq!(accepted[0].path, "/v1/profiles/41");
        assert_eq!(
            accepted[0].body["profile"]["attributes"],
            json!({"plan": TRAIT_VALUE, "country": null}),
            "the key trait is not sent unless the node lists it"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_429_waits_for_the_retry_after_and_the_retry_reuses_the_idempotency_key(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        let key = env.queue(&enrolled, "upsert", 5).await;
        env.fake.script(FakeProviderScript::Status {
            status: 429,
            body: json!({}),
            retry_after: Some(17),
        });

        let limited = env.attempt().await.unwrap_err();
        let after_first =
            env.events("sync.succeeded").await.len() + env.events("sync.failed").await.len();
        env.attempt().await.unwrap();

        assert_eq!(limited.retry_after(), Some(Duration::from_secs(17)));
        assert_eq!(after_first, 0, "a retried attempt writes no outcome");
        assert_eq!(env.fake.requests(), 2);
        let accepted = env.fake.accepted();
        assert_eq!(accepted.len(), 1, "one change at a key-honouring provider");
        assert_eq!(accepted[0].key, key);
        assert_eq!(env.events("sync.succeeded").await[0]["attempts"], json!(2));
        assert_eq!(
            env.attempts().await,
            [
                ("sync".to_owned(), "throttled".to_owned(), 1),
                ("sync".to_owned(), "delivered".to_owned(), 2)
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_4xx_is_terminal_and_the_last_attempt_of_a_5xx_is_recorded_as_a_failure(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        env.queue(&enrolled, "upsert", 1).await;
        env.fake.script(FakeProviderScript::Status {
            status: 422,
            body: json!({}),
            retry_after: None,
        });
        env.attempt().await.unwrap();
        env.queue(&enrolled, "remove", 1).await;
        env.fake.script(FakeProviderScript::Status {
            status: 503,
            body: json!({}),
            retry_after: None,
        });
        env.attempt().await.unwrap();

        let failed = env.events("sync.failed").await;
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0]["status"], json!(422));
        assert_eq!(failed[1]["status"], json!(503));
        assert_eq!(
            env.decisions().await,
            [
                (
                    "sync.remove".to_owned(),
                    "failed".to_owned(),
                    Some("http_5xx".to_owned())
                ),
                (
                    "sync.upsert".to_owned(),
                    "failed".to_owned(),
                    Some("http_4xx".to_owned())
                )
            ]
        );
        assert!(env.events("sync.succeeded").await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_who_is_gone_or_exited_gets_no_call_and_no_outcome(pool: PgPool) {
        let env = Env::new(pool).await;
        let person = env.person().await;
        let exited = env.enroll(person).await;
        env.queue(&exited, "upsert", 5).await;
        sqlx::query("UPDATE enrollments SET state = 'exited', exit_reason = 'goal', ended_at = now() WHERE id = $1")
            .bind(exited.enrollment)
            .execute(&env.pool)
            .await
            .unwrap();
        env.attempt().await.unwrap();

        let erased = env.person().await;
        let other = env.enroll(erased).await;
        env.queue(&other, "remove", 5).await;
        sqlx::query("DELETE FROM enrollments WHERE id = $1")
            .bind(other.enrollment)
            .execute(&env.pool)
            .await
            .unwrap();
        env.attempt().await.unwrap();

        assert_eq!(env.fake.requests(), 0);
        assert!(env.decisions().await.is_empty());
        assert!(env.events("sync.failed").await.is_empty());
        assert!(env.events("sync.succeeded").await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_operation_the_manifest_lacks_and_an_archived_destination_end_with_a_reason(
        pool: PgPool,
    ) {
        let env = Env::with(
            pool,
            |mut manifest| {
                manifest["destination"]
                    .as_object_mut()
                    .unwrap()
                    .remove("remove_profile");
                manifest
            },
            100,
            100,
        )
        .await;
        let person = env.person().await;
        let enrolled = env.enroll(person).await;
        env.queue(&enrolled, "remove", 5).await;
        env.attempt().await.unwrap();
        env.queue(&enrolled, "upsert", 5).await;
        sqlx::query("UPDATE destinations SET archived_at = now() WHERE id = $1")
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();
        env.attempt().await.unwrap();

        let reasons: Vec<Option<String>> = env.decisions().await.into_iter().map(|d| d.2).collect();
        assert_eq!(
            reasons,
            [
                Some("operation_unsupported".to_owned()),
                Some("destination_unavailable".to_owned())
            ]
        );
        assert_eq!(env.fake.requests(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_empty_rate_bucket_re_enqueues_the_sync_without_spending_an_attempt(pool: PgPool) {
        let env = Env::with(pool, |manifest| manifest, 1, 1).await;
        let person = env.person().await;
        let first = env.enroll(person).await;
        let second = env.enroll(env.person().await).await;
        env.queue(&first, "upsert", 5).await;
        env.queue(&second, "upsert", 5).await;

        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        assert_eq!(env.fake.requests(), 1, "the second sync waits for a token");
        assert_eq!(env.events("sync.succeeded").await.len(), 1);
        let (waits, attempts, base): (i64, i32, i32) = sqlx::query_as(
            "SELECT count(*), max(max_attempts), max((payload ->> 'attempt_base')::int) \
             FROM jobs WHERE kind = 'delivery.sync' AND unique_key LIKE '%:wait:1'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!((waits, attempts, base), (1, 5, 0));
    }

    /// A second sync that must wait for a token, its job naming a person as `flow.step` writes
    /// it (the erasure purges by that name). The one token the bucket has is spent first.
    async fn waiting_sync_of_a_person(env: &Env) -> Uuid {
        let first = env.enroll(env.person().await).await;
        env.queue(&first, "upsert", 5).await;
        env.attempt().await.unwrap();
        let person = env.person().await;
        let second = env.enroll(person).await;
        let key = env.queue(&second, "upsert", 5).await;
        sqlx::query(
            "UPDATE jobs SET payload = jsonb_set(payload, '{person_id}', to_jsonb($2::text)) \
             WHERE unique_key = $1",
        )
        .bind(key)
        .bind(person.to_string())
        .execute(&env.pool)
        .await
        .unwrap();
        person
    }

    async fn sync_jobs(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'delivery.sync'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn wait_until_something_waits_for_a_lock(pool: &PgPool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND cardinality(pg_blocking_pids(pid)) > 0)",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "nothing waited for a lock"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rate_wait_that_meets_an_erasure_in_flight_enqueues_no_copy(pool: PgPool) {
        let env = Env::with(pool, |manifest| manifest, 1, 1).await;
        let person = waiting_sync_of_a_person(&env).await;
        // The worker has the job (an erasure marks a running job instead of deleting it).
        let claimed = env.claim().await.expect("the waiting sync is ready");
        let trace = TraceContext::root(Cause::api("erase"));
        // The erasure has deleted the person and purged the jobs, and has not committed.
        let mut erasure = zirv_flow_store::begin(&env.pool).await.unwrap();
        zirv_flow_store::delete_person(
            &mut erasure,
            env.ws,
            person,
            zirv_flow_store::DeleteReason::Erasure,
            &trace,
        )
        .await
        .unwrap();

        let commit_when_the_handler_waits = async {
            wait_until_something_waits_for_a_lock(&env.pool).await;
            zirv_flow_store::commit(erasure).await.unwrap();
        };
        let (result, ()) =
            tokio::join!(env.attempt_claimed(claimed), commit_when_the_handler_waits);

        result.unwrap();
        assert_eq!(
            sync_jobs(&env.pool).await,
            0,
            "no job names the erased person"
        );
        assert_eq!(env.fake.requests(), 1, "the waiting sync made no call");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_that_follows_a_rate_wait_purges_the_copy_it_enqueued(pool: PgPool) {
        let env = Env::with(pool, |manifest| manifest, 1, 1).await;
        let person = waiting_sync_of_a_person(&env).await;
        let claimed = env.claim().await.expect("the waiting sync is ready");
        let mut handler = zirv_flow_store::begin(&env.pool).await.unwrap();
        env.run(&mut handler, &claimed).await.unwrap();
        let copies: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE unique_key LIKE '%:wait:1'")
                .fetch_one(&mut *handler)
                .await
                .unwrap();
        assert_eq!(
            copies, 1,
            "the wait copy is enqueued in the handler's transaction"
        );

        // The erasure starts while the handler holds the person, and waits for it.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (pool, ws) = (env.pool.clone(), env.ws);
        let erasure = tokio::spawn(async move {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sender.send(pid).unwrap();
            let trace = TraceContext::root(Cause::api("erase"));
            zirv_flow_store::delete_person(
                &mut tx,
                ws,
                person,
                zirv_flow_store::DeleteReason::Erasure,
                &trace,
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        });
        let pid = receiver.await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !sqlx::query_scalar::<_, bool>("SELECT cardinality(pg_blocking_pids($1)) > 0")
            .bind(pid)
            .fetch_one(&env.pool)
            .await
            .unwrap()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the erasure never waited"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        zirv_flow_store::complete_job(&mut *handler, claimed.id, claimed.lease_token)
            .await
            .unwrap();
        zirv_flow_store::commit(handler).await.unwrap();
        erasure.await.unwrap();

        assert_eq!(
            sync_jobs(&env.pool).await,
            0,
            "the copy went with the erasure"
        );
    }

    fn payload() -> Value {
        json!({
            "workspace_id": WorkspaceId::new().to_string(),
            "flow_id": Uuid::from_u128(1),
            "version": 1,
            "node_id": "push",
            "enrollment_id": Uuid::from_u128(2),
            "person_id": Uuid::from_u128(3),
            "trace_id": zirv_flow_core::TraceId::new().to_string(),
            "idempotency_key": "e:push:sync",
            "sync_id": Uuid::from_u128(4),
            "destination_id": Uuid::from_u128(5),
            "operation": "upsert",
            "trait_keys": ["plan"],
        })
    }

    #[test]
    fn the_kind_is_registered_once_under_its_name() {
        assert_eq!(DeliverySync::KIND, "delivery.sync");
        let kinds = crate::Registry::built_in().kinds();
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "delivery.sync")
                .count(),
            1
        );
    }

    #[test]
    fn the_payload_flow_step_enqueues_parses_and_defaults_the_wait_fields() {
        let job = SyncJob::parse(&payload()).unwrap();

        assert_eq!(job.operation, SyncOperation::Upsert);
        assert_eq!(job.trait_keys, ["plan"]);
        assert_eq!((job.attempt_base, job.deferrals), (0, 0));
        assert!(job.first_wait.is_none());
    }

    #[test]
    fn a_payload_with_an_unknown_operation_or_a_missing_part_is_refused() {
        let mut other = payload();
        other["operation"] = json!("merge");
        let mut missing = payload();
        missing.as_object_mut().unwrap().remove("sync_id");
        let mut keys = payload();
        keys["trait_keys"] = json!([1]);

        assert!(SyncJob::parse(&other).is_none());
        assert!(SyncJob::parse(&missing).is_none());
        assert!(SyncJob::parse(&keys).is_none());
    }

    #[test]
    fn a_trait_is_a_key_when_it_is_a_non_empty_string_or_a_number() {
        assert_eq!(key_text(&json!(" k-1 ")).as_deref(), Some("k-1"));
        assert_eq!(key_text(&json!(42)).as_deref(), Some("42"));
        for value in [
            json!(""),
            json!("  "),
            json!(null),
            json!(true),
            json!(["a"]),
        ] {
            assert_eq!(key_text(&value), None, "{value}");
        }
    }

    #[test]
    fn a_sync_with_no_usable_destination_ends_with_a_fixed_reason() {
        assert!(matches!(
            plan(None, SyncOperation::Upsert),
            Err("destination_unavailable")
        ));
    }

    #[test]
    fn an_ending_with_no_call_is_a_terminal_verdict_with_its_reason() {
        let done = ended("missing_key");

        assert_eq!(
            done.verdict,
            Verdict::Terminal {
                status: None,
                reason: "missing_key"
            }
        );
    }
}
