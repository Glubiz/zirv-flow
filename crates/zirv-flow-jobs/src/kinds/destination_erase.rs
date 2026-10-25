//! `destination.erase`: the erase call to one destination when a person is erased (architecture
//! 16.3, "As built (erase calls, #227)").
//!
//! `privacy.erase` plans the call inside the erasure transaction: it writes the pending
//! `erasure_destination_calls` row and enqueues this job with `{workspace_id, ledger_id,
//! destination_id, external_key}`. The external key is the identifier the destination was given
//! for the person (the person id, an identity or a trait value, as the destination declares); no
//! other trait value is in the payload. The erasure is committed already, so nothing here blocks
//! or undoes it.
//!
//! - The call goes through the shared delivery path: the destination's rate bucket admits it, the
//!   verdict is the one every outbound job uses, and the idempotency key is
//!   [`idempotency_key`], stable for the ledger and destination, so a retry cannot duplicate.
//! - A 2xx records `done`. A retryable failure backs off as the other delivery kinds do; a
//!   terminal failure, or the last attempt of a retryable one, records `failed` with the fixed
//!   code of the verdict, and the job ends as done. A destination that was deleted or archived
//!   before the job ran records `failed` with `destination_unavailable`.
//! - **The payload does not outlive the job.** A job that ends is deleted by the worker
//!   (`complete_job`), and one that dies is scrubbed of the key by the store (`fail_job` and
//!   `reap_expired`), so a dead job cannot keep the key. A retried dead job has no key: it
//!   records `failed` with `key_unavailable`.
//!
//! The external key reaches the outbound request only: never an event, a decision, a log line or
//! a metric.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{DestinationContext, Registry};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    Admission, Destination, ERASE_CALL_JOB_KIND, EXTERNAL_KEY_FIELD, PgPool, find_destination,
    finish_erasure_call, idempotency_key, throttle_bucket,
};

use crate::delivery::credential::open_credential;
use crate::delivery::{self, Verdict};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct DestinationErase;

/// The code of a call whose destination was deleted, archived or cannot be reached by this build.
const DESTINATION_UNAVAILABLE: &str = "destination_unavailable";
/// The code of a call whose destination no longer declares erase.
const ERASE_UNSUPPORTED: &str = "erase_unsupported";
/// The code of a call whose job lost its key (a dead job that was retried).
const KEY_UNAVAILABLE: &str = "key_unavailable";
/// The code of a call that kept waiting for the destination's rate bucket.
const RATE_LIMITED: &str = "rate_limited";

/// The payload `privacy.erase` plans, through the store.
struct EraseCall {
    workspace: WorkspaceId,
    ledger: Uuid,
    destination: Uuid,
    /// None for a job that died and was retried: the store scrubbed the key.
    key: Option<String>,
}

impl EraseCall {
    fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let id = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            ledger: id("ledger_id")?,
            destination: id("destination_id")?,
            key: text(EXTERNAL_KEY_FIELD)
                .filter(|key| !key.is_empty())
                .map(str::to_owned),
        })
    }
}

/// What the call needs, or the fixed code of why it cannot be made.
fn plan(
    found: Option<Destination>,
) -> Result<
    (
        Destination,
        &'static dyn zirv_flow_connectors::destinations::ErasedDestination,
    ),
    &'static str,
> {
    let destination = found
        .filter(|destination| destination.archived_at.is_none())
        .ok_or(DESTINATION_UNAVAILABLE)?;
    let driver = Registry::built_in()
        .get(&destination.kind)
        .ok_or(DESTINATION_UNAVAILABLE)?;
    // The same decision `privacy.erase` made when it planned the call.
    if !super::privacy_erase::declares_erase(driver.spec(), &destination.config) {
        return Err(ERASE_UNSUPPORTED);
    }
    Ok((destination, driver))
}

impl JobKind for DestinationErase {
    const KIND: &'static str = ERASE_CALL_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = EraseCall::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not an erase call"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let attempts = ctx.attempts();
        let last_attempt = attempts >= ctx.max_attempts();

        let Some(key) = job.key.as_deref() else {
            return settle(ctx, &job, Err(KEY_UNAVAILABLE)).await;
        };
        let found = find_destination(ctx.conn(), job.workspace, job.destination).await?;
        let (destination, driver) = match plan(found) {
            Ok(plan) => plan,
            Err(code) => return settle(ctx, &job, Err(code)).await,
        };
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let pool = ctx
            .pool()
            .cloned()
            .ok_or_else(|| JobError::new("the worker has no pool"))?;

        match ctx
            .rate_gates()
            .clone()
            .admit(&pool, destination.id, &clock)
            .await?
        {
            Admission::Granted => {}
            Admission::Wait(wait) => {
                // The wait spends an attempt: the job has attempts to spare for it, and a bucket
                // that stays empty ends the call as failed instead of waiting for ever.
                if last_attempt {
                    return settle(ctx, &job, Err(RATE_LIMITED)).await;
                }
                return Err(
                    JobError::new("the destination's rate bucket is empty").with_retry_after(wait)
                );
            }
            Admission::Gone => return settle(ctx, &job, Err(DESTINATION_UNAVAILABLE)).await,
        }

        let credential = match open_credential(ctx, job.workspace, &destination).await? {
            Ok(credential) => credential,
            Err(verdict) => return conclude(ctx, &job, verdict, None).await,
        };
        // No redirects: a redirect would turn the call into something that was never the erase.
        let limits = HttpLimits {
            max_redirects: 0,
            ..HttpLimits::default()
        };
        let context = DestinationContext::new(
            Arc::clone(ctx.egress()),
            limits,
            driver.spec().kind,
            destination.id.to_string(),
        )
        .map_err(|_| JobError::new("the outbound client could not be built"))?;
        let idempotency = idempotency_key(job.ledger, job.destination);
        let done = delivery::erase(
            driver,
            &context,
            &destination.config,
            &credential,
            (key, &idempotency),
            now,
        )
        .await;
        conclude(ctx, &job, done.verdict, Some((&pool, &destination, &clock))).await
    }
}

/// Records the verdict, or hands a retryable one back to the worker.
async fn conclude(
    ctx: &mut JobContext<'_>,
    job: &EraseCall,
    verdict: Verdict,
    bucket: Option<(&PgPool, &Destination, &zirv_flow_store::Clock)>,
) -> Result<(), JobError> {
    let last_attempt = ctx.attempts() >= ctx.max_attempts();
    match verdict {
        Verdict::Succeeded { .. } => settle(ctx, job, Ok(())).await,
        Verdict::Terminal { reason, .. } => settle(ctx, job, Err(reason)).await,
        Verdict::Retry {
            reason,
            message,
            retry_after,
            ..
        } => {
            if let (Some((pool, destination, clock)), "http_429") = (bucket, reason)
                && throttle_bucket(pool, destination.id, retry_after, clock)
                    .await
                    .is_err()
            {
                warn!(destination = %destination.id, "a destination's rate could not be slowed");
            }
            if last_attempt {
                return settle(ctx, job, Err(reason)).await;
            }
            warn!(
                destination = %job.destination,
                attempt = ctx.attempts(),
                reason,
                "an erase call will be retried"
            );
            let error = JobError::new(message);
            Err(retry_after.map_or(error, |wait: Duration| error.with_retry_after(wait)))
        }
    }
}

/// Writes the call's result; the job then ends as done and the worker deletes it with its payload.
async fn settle(
    ctx: &mut JobContext<'_>,
    job: &EraseCall,
    result: Result<(), &str>,
) -> Result<(), JobError> {
    let attempts = ctx.attempts();
    if let Err(code) = result {
        warn!(
            destination = %job.destination,
            attempt = attempts,
            reason = code,
            "an erase call failed"
        );
    }
    finish_erasure_call(
        ctx.conn(),
        job.workspace,
        (job.ledger, job.destination),
        result,
        attempts,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_connectors::destinations::{
        FakeErased, FakeProvider, FakeProviderOptions, fake_ledger_erased,
    };
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::{
        Cause, HmacKey, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{
        ClaimedJob, EraseCallOutcome, ProfileOutcome, ProfileRecord, apply_profile_batch,
        list_erasure_calls,
    };

    use super::*;
    use crate::context::IngestSettings;
    use crate::kinds::privacy_erase::PrivacyErase;

    const EMAIL: &str = "ada@example.com";
    const TRAIT_VALUE: &str = "SENTINEL-TRAIT-VALUE";
    const PROFILES: &str =
        include_str!("../../../zirv-flow-connectors/tests/fixtures/providers/profile_sync.json");
    const EMAIL_PROVIDER: &str = include_str!(
        "../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json"
    );

    /// A manifest for `fake`'s base URL that erases by `POST /erasures` with the key in the body.
    fn manifest_for(fixture: &str, fake: &FakeProvider) -> Value {
        let mut manifest: Value = serde_json::from_str(fixture).unwrap();
        manifest["base_url"] = json!(fake.base_url());
        manifest["auth"] = json!({"type": "none"});
        if manifest["destination"].get("feedback").is_some() {
            manifest["auth"]["credential"] = json!({"fields": [
                {"key": "feedback_secret", "label": "Feedback signing secret", "type": "string",
                 "required": false, "secret": true}
            ]});
        }
        manifest["destination"]["erase_profile"] = json!({
            "method": "POST",
            "path": "/erasures",
            "idempotency_header": "Idempotency-Key",
            "key_from": {"identity": "email"},
            "body": {"key": "/subject"}
        });
        manifest
    }

    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        person: Uuid,
        key: Arc<HmacKey>,
    }

    impl Env {
        async fn new(pool: PgPool) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse("ours").unwrap(),
                &Slug::parse("ours").unwrap(),
            )
            .await
            .unwrap()
            .id;
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: "1".into(),
                identifiers: vec![("email".into(), EMAIL.into())],
                traits: json!({"plan": TRAIT_VALUE}).as_object().cloned().unwrap(),
                observed_at: datetime!(2026-01-01 00:00 UTC),
                trace: TraceContext::root(Cause::api("t")),
            };
            let outcomes = apply_profile_batch(&pool, ws, &[record]).await.unwrap();
            let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
                panic!("not applied");
            };
            let key = Arc::new(
                HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Identifier),
            );
            Self {
                pool,
                ws,
                person: person_id,
                key,
            }
        }

        async fn destination(&self, kind: &str, name: &str, config: Value) -> Uuid {
            let id = Uuid::now_v7();
            zirv_flow_store::create_destination(
                &self.pool,
                self.ws,
                &zirv_flow_store::NewDestination {
                    id,
                    kind,
                    name,
                    config: &config,
                    credential_id: None,
                    rate: 100,
                    burst: 100,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            id
        }

        /// A forwarding item of the person at the destination.
        async fn forwarded(&self, destination: Uuid, state: &str) {
            sqlx::query(
                "INSERT INTO forwarding_outbox (id, workspace_id, destination_id, rule_id, \
                    event_id, person_id, idempotency_key, payload, state) \
                 VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), $3, \
                    gen_random_uuid()::text, '{}', $4)",
            )
            .bind(self.ws.as_uuid())
            .bind(destination)
            .bind(self.person)
            .bind(state)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// A message of the person through the destination.
        async fn messaged(&self, destination: Uuid) {
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, reason, completed_at) \
                 VALUES (gen_random_uuid(), $1, $3, gen_random_uuid(), 'n', gen_random_uuid(), \
                    $2, 'skipped', 'no_consent', now())",
            )
            .bind(self.ws.as_uuid())
            .bind(destination)
            .bind(self.person)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// A profile sync outcome of the person at the destination.
        async fn synced(&self, destination: Uuid) {
            sqlx::query(
                "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, \
                    received_at, properties, source, trace_id, validation_state) \
                 VALUES (gen_random_uuid(), $1, $3, 'sync.succeeded', now(), now(), \
                    jsonb_build_object('destination_id', $2::text), 'flows', gen_random_uuid(), \
                    'valid')",
            )
            .bind(self.ws.as_uuid())
            .bind(destination)
            .bind(self.person)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// Requests the erasure and runs `privacy.erase`; returns the ledger id.
        async fn erase(&self) -> Uuid {
            let trace = TraceContext::root(Cause::api("t"));
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let request = zirv_flow_store::request_erasure(
                &mut tx,
                self.ws,
                self.person,
                &self.key,
                None,
                &trace,
            )
            .await
            .unwrap()
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            let claimed = self.claim(PrivacyErase::KIND).await.unwrap();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let mut ctx =
                JobContext::new(&mut tx, &claimed).with_identifier_key(Some(Arc::clone(&self.key)));
            PrivacyErase
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            request.id
        }

        async fn claim(&self, kind: &str) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(&self.pool, "test", &[kind], Duration::from_secs(30), None)
                .await
                .unwrap()
        }

        /// Claims one erase call job and runs one attempt as a worker does: the work commits with
        /// the job's completion, and an error rolls it back and puts the job up again at once.
        async fn attempt(&self) -> Option<Result<(), JobError>> {
            let claimed = self.claim(DestinationErase::KIND).await?;
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let egress = Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap());
            let result = {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_outbound(None, egress, Arc::default())
                    .with_ingest(IngestSettings {
                        pool: self.pool.clone(),
                        redaction_key: None,
                        batch_size: 1,
                        shards: 1,
                    });
                DestinationErase
                    .run(&mut ctx, claimed.payload.clone())
                    .await
            };
            match result {
                Ok(()) => {
                    zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                        .await
                        .unwrap();
                    zirv_flow_store::commit(tx).await.unwrap();
                    Some(Ok(()))
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
                    Some(Err(error))
                }
            }
        }

        async fn queued(&self) -> Vec<Value> {
            sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = $1 ORDER BY unique_key")
                .bind(DestinationErase::KIND)
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }

        async fn calls(&self, ledger: Uuid) -> Vec<zirv_flow_store::ErasureCall> {
            list_erasure_calls(&self.pool, self.ws, &[ledger])
                .await
                .unwrap()
        }
    }

    #[test]
    fn the_kind_name_is_destination_erase() {
        assert_eq!(DestinationErase::KIND, "destination.erase");
    }

    #[test]
    fn the_payload_needs_a_workspace_a_ledger_and_a_destination_and_the_key_is_optional() {
        let (ws, ledger, destination) = (WorkspaceId::new(), Uuid::now_v7(), Uuid::now_v7());
        let base = json!({"workspace_id": ws, "ledger_id": ledger, "destination_id": destination});

        let without = EraseCall::parse(&base).unwrap();
        let mut with = base.clone();
        with["external_key"] = json!("k");

        assert!(without.key.is_none());
        assert_eq!(EraseCall::parse(&with).unwrap().key.as_deref(), Some("k"));
        assert!(EraseCall::parse(&json!({"ledger_id": ledger})).is_none());
        assert!(EraseCall::parse(&json!({"workspace_id": ws, "ledger_id": 5})).is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_forwarding_a_sync_and_a_delivery_destination_each_get_one_erase_call_with_a_stable_key(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let ledger_name = format!("erase-{}", Uuid::now_v7());
        let provider_sync = FakeProvider::start(FakeProviderOptions::default()).await;
        let provider_mail = FakeProvider::start(FakeProviderOptions::default()).await;
        let forwarding = env
            .destination("fake", "forwarding", json!({"ledger": ledger_name}))
            .await;
        let sync = env
            .destination(
                "provider",
                "sync",
                json!({"manifest": manifest_for(PROFILES, &provider_sync)}),
            )
            .await;
        let delivery = env
            .destination(
                "provider",
                "delivery",
                json!({"manifest": manifest_for(EMAIL_PROVIDER, &provider_mail)}),
            )
            .await;
        let hook = env
            .destination(
                "webhook",
                "hook",
                json!({"url": "https://hooks.example.test/x"}),
            )
            .await;
        env.forwarded(forwarding, "dead").await;
        env.synced(sync).await;
        env.messaged(delivery).await;
        env.messaged(hook).await;

        let ledger = env.erase().await;

        let queued = env.queued().await;
        assert_eq!(
            queued.len(),
            3,
            "none for the webhook, which declares no erase"
        );
        let keys: Vec<&str> = queued
            .iter()
            .map(|payload| payload["external_key"].as_str().unwrap())
            .collect();
        for payload in &queued {
            assert!(!payload.to_string().contains(TRAIT_VALUE), "{payload}");
            assert_ne!(payload["destination_id"], json!(hook));
        }
        assert_eq!(keys.iter().filter(|key| **key == EMAIL).count(), 2);
        assert!(keys.contains(&env.person.to_string().as_str()));

        while let Some(result) = env.attempt().await {
            result.unwrap();
        }

        assert_eq!(
            fake_ledger_erased(&ledger_name),
            [FakeErased {
                key: idempotency_key(ledger, forwarding),
                subject: env.person.to_string()
            }]
        );
        for (fake, destination) in [(&provider_sync, sync), (&provider_mail, delivery)] {
            let accepted = fake.accepted();
            assert_eq!(accepted.len(), 1);
            assert_eq!(accepted[0].key, idempotency_key(ledger, destination));
            assert_eq!(accepted[0].path, "/erasures");
            assert_eq!(accepted[0].body, json!({"subject": EMAIL}));
        }
        let calls = env.calls(ledger).await;
        assert_eq!(calls.len(), 3);
        assert!(
            calls
                .iter()
                .all(|call| call.outcome == EraseCallOutcome::Done && call.attempts == 1)
        );
        assert!(
            env.queued().await.is_empty(),
            "the payloads are gone with the jobs"
        );
        assert!(env.attempt().await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_destination_without_the_capability_gets_no_job_and_no_row(pool: PgPool) {
        let env = Env::new(pool).await;
        let hook = env
            .destination(
                "webhook",
                "hook",
                json!({"url": "https://hooks.example.test/x"}),
            )
            .await;
        let plain = FakeProvider::start(FakeProviderOptions::default()).await;
        let mut manifest = manifest_for(PROFILES, &plain);
        manifest["destination"]
            .as_object_mut()
            .unwrap()
            .remove("erase_profile");
        let provider = env
            .destination("provider", "plain", json!({"manifest": manifest}))
            .await;
        env.messaged(hook).await;
        env.synced(provider).await;

        let ledger = env.erase().await;

        assert!(env.queued().await.is_empty());
        assert!(env.calls(ledger).await.is_empty());
        assert_eq!(plain.requests(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_destination_retries_then_records_failed_and_the_erasure_completes(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let ledger_name = format!("erase-{}", Uuid::now_v7());
        let failing = env
            .destination(
                "fake",
                "failing",
                json!({"ledger": ledger_name, "mode": "server_error"}),
            )
            .await;
        env.forwarded(failing, "pending").await;
        let ledger = env.erase().await;

        let mut results = Vec::new();
        while let Some(result) = env.attempt().await {
            results.push(result);
        }

        let attempts = usize::try_from(zirv_flow_store::ERASE_CALL_ATTEMPTS).unwrap();
        assert_eq!(results.len(), attempts);
        assert!(results[..attempts - 1].iter().all(Result::is_err));
        assert!(
            results[attempts - 1].is_ok(),
            "the last attempt ends the job as done"
        );
        assert_eq!(fake_ledger_erased(&ledger_name).len(), attempts);
        let calls = env.calls(ledger).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].outcome, EraseCallOutcome::Failed);
        assert_eq!(calls[0].error_code.as_deref(), Some("http_5xx"));
        assert_eq!(usize::try_from(calls[0].attempts).unwrap(), attempts);
        assert!(env.queued().await.is_empty());
        let request = zirv_flow_store::find_erasure_request(&env.pool, env.ws, ledger)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.status, zirv_flow_store::ErasureStatus::Completed);
        assert!(
            zirv_flow_store::find_person(&env.pool, env.ws, env.person)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_terminal_answer_records_failed_at_once(pool: PgPool) {
        let env = Env::new(pool).await;
        let rejecting = env
            .destination(
                "fake",
                "rejecting",
                json!({"ledger": format!("erase-{}", Uuid::now_v7()), "mode": "rejected"}),
            )
            .await;
        env.forwarded(rejecting, "pending").await;
        let ledger = env.erase().await;

        env.attempt().await.unwrap().unwrap();

        let calls = env.calls(ledger).await;
        assert_eq!(calls[0].outcome, EraseCallOutcome::Failed);
        assert_eq!(calls[0].error_code.as_deref(), Some("http_4xx"));
        assert_eq!(calls[0].attempts, 1);
        assert!(env.attempt().await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_destination_archived_before_the_job_ran_records_failed_with_a_fixed_code(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let gone = env
            .destination(
                "fake",
                "gone",
                json!({"ledger": format!("erase-{}", Uuid::now_v7())}),
            )
            .await;
        env.forwarded(gone, "pending").await;
        let ledger = env.erase().await;
        zirv_flow_store::archive_destination(&env.pool, env.ws, gone)
            .await
            .unwrap();

        env.attempt().await.unwrap().unwrap();

        let calls = env.calls(ledger).await;
        assert_eq!(calls[0].outcome, EraseCallOutcome::Failed);
        assert_eq!(
            calls[0].error_code.as_deref(),
            Some(DESTINATION_UNAVAILABLE)
        );
        assert!(env.queued().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_that_lost_its_key_makes_no_call_and_records_failed(pool: PgPool) {
        let env = Env::new(pool).await;
        let ledger_name = format!("erase-{}", Uuid::now_v7());
        let destination = env
            .destination("fake", "keyed", json!({"ledger": ledger_name}))
            .await;
        env.forwarded(destination, "pending").await;
        let ledger = env.erase().await;
        sqlx::query("UPDATE jobs SET payload = payload - 'external_key' WHERE kind = $1")
            .bind(DestinationErase::KIND)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap().unwrap();

        let calls = env.calls(ledger).await;
        assert_eq!(calls[0].error_code.as_deref(), Some(KEY_UNAVAILABLE));
        assert!(fake_ledger_erased(&ledger_name).is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_of_another_workspace_or_with_a_broken_payload_fails_the_job(pool: PgPool) {
        let env = Env::new(pool).await;
        let mut job = zirv_flow_store::NewJob::new(
            DestinationErase::KIND,
            TraceContext::root(Cause::api("t")),
        );
        job.workspace_id = Some(env.ws);
        job.payload = json!({"workspace_id": WorkspaceId::new(), "ledger_id": Uuid::now_v7(),
            "destination_id": Uuid::now_v7(), "external_key": "k"});
        zirv_flow_store::enqueue(&env.pool, &job).await.unwrap();

        let other = env.attempt().await.unwrap().unwrap_err();

        assert_eq!(
            other.message(),
            "the payload names another workspace than the job"
        );
        sqlx::query("DELETE FROM jobs WHERE kind = $1")
            .bind(DestinationErase::KIND)
            .execute(&env.pool)
            .await
            .unwrap();
        let mut broken = zirv_flow_store::NewJob::new(
            DestinationErase::KIND,
            TraceContext::root(Cause::api("t")),
        );
        broken.workspace_id = Some(env.ws);
        broken.unique_key = Some("broken".into());
        zirv_flow_store::enqueue(&env.pool, &broken).await.unwrap();
        let error = env.attempt().await.unwrap().unwrap_err();
        assert_eq!(error.message(), "the payload is not an erase call");
    }
}
