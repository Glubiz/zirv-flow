//! `event.forward`: delivers the forwarding outbox rows of one destination (architecture 12.5
//! and 14).
//!
//! There is no job per event. The transaction that writes a matching event inserts the outbox row
//! and makes sure one `event.forward` job of the destination is ready (`ensure_forward_job`). The
//! job claims a bounded batch of due rows, delivers each through the shared delivery code with the
//! row's stable idempotency key, and records how each attempt went:
//!
//! - **A 2xx answer** marks the row `delivered` and empties its payload.
//! - **A retryable verdict** (408, 429, 5xx, a timeout, a transport error) sets the row's next
//!   attempt with the worker's backoff, or after the `Retry-After` when that is longer, and appends
//!   an `event.forward` decision with the reason code.
//! - **A terminal verdict, or the last attempt**, marks the row `dead` with the same decision.
//!   A dead row stays until an operator re-drives it.
//!
//! A claim spends no lock across the call: it counts the attempt and pushes the row's next attempt
//! a lease into the future, so a worker that dies leaves the row to be taken again, and an
//! erasure never waits on a delivery. The outcome write is fenced on the attempt number, so a row
//! that was re-driven or erased meanwhile is left alone.
//!
//! When due rows remain, or rows wait for a retry, the job leaves its successor behind. It never
//! writes an event and never enqueues flow arrivals. Every `delivery_attempts` row, metric and
//! decision names ids and fixed codes only; the payload reaches none of them.

use std::time::Duration;

use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    DestinationContext, ErasedDestination, Outbound, Registry,
};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::decision_actions;
use zirv_flow_core::schedule::backoff;
use zirv_flow_store::{
    Admission, ClaimedRow, DELIVERED_RETENTION, Destination, FORWARD_JOB_KIND, NewDecision, PgPool,
    Settled, SubjectKind, append_decision, claim_due, defer_row, ensure_forward_job, extend_lease,
    find_destination, next_due, purge_delivered, settle_row, throttle_bucket,
};

use super::workspace_and_id;
use crate::delivery::attempt::AttemptTarget;
use crate::delivery::credential::{Usage, open_credential};
use crate::delivery::{self, Attempt, Verdict};
use crate::worker::{random_unit, retry_delay};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct EventForward;

/// Rows claimed per round.
const BATCH: i64 = 25;
/// Rounds per run; more due rows are left to the successor job.
const ROUNDS: usize = 4;
/// How long a claim holds a row before another worker may take it.
const LEASE: Duration = Duration::from_secs(300);
/// Attempts of one cycle (before a re-drive) after which a retryable failure is a dead letter.
const MAX_ATTEMPTS: i32 = 8;
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// How long a row waits when its destination has no rate-limit gate to ask.
const GONE_WAIT: Duration = Duration::from_secs(60);
/// Delivered rows purged per run.
const PURGE_LIMIT: i64 = 1000;
/// The channel a destination must declare to carry forwarded events.
const CHANNEL: &str = "event";

/// What an attempt's verdict does to its row.
#[derive(Debug, Eq, PartialEq)]
enum Plan {
    Delivered,
    Retry {
        at: OffsetDateTime,
        reason: &'static str,
    },
    Dead {
        reason: &'static str,
    },
}

/// Decides the row's next step from the verdict of its `attempts`-th attempt of the cycle.
fn plan(verdict: &Verdict, attempts: i32, now: OffsetDateTime, unit: f64) -> Plan {
    match verdict {
        Verdict::Succeeded { .. } => Plan::Delivered,
        Verdict::Terminal { reason, .. } => Plan::Dead { reason },
        Verdict::Retry {
            reason,
            retry_after,
            ..
        } => {
            if attempts >= MAX_ATTEMPTS {
                return Plan::Dead { reason };
            }
            let delay = retry_delay(
                backoff(
                    u32::try_from(attempts).unwrap_or(u32::MAX),
                    BACKOFF_BASE,
                    BACKOFF_CAP,
                    unit,
                ),
                *retry_after,
                BACKOFF_CAP,
            );
            Plan::Retry {
                at: now + delay,
                reason,
            }
        }
    }
}

impl JobKind for EventForward {
    const KIND: &'static str = FORWARD_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, destination_id) = workspace_and_id(
            &payload,
            "destination_id",
            "the payload is not an event forward",
        )?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new("the worker has no pool to forward events on"));
        };
        // A destination that is gone or archived has nothing to deliver to. Its rows stay.
        let Some(destination) = find_destination(ctx.conn(), workspace, destination_id).await?
        else {
            return Ok(());
        };
        if destination.archived_at.is_some() {
            return Ok(());
        }
        let registry = Registry::built_in();
        let driver = registry
            .get(&destination.kind)
            .filter(|driver| driver.spec().capabilities.channels.contains(&CHANNEL));
        purge_delivered(&pool, destination_id, DELIVERED_RETENTION, PURGE_LIMIT).await?;

        let mut credential: Option<Result<Credential, Verdict>> = None;
        for _ in 0..ROUNDS {
            let claimed = claim(&pool, workspace, destination_id).await?;
            if claimed.is_empty() {
                break;
            }
            for row in claimed {
                forward_one(ctx, &pool, &destination, driver, &mut credential, &row).await?;
            }
        }

        if let Some(due) = next_due(ctx.conn(), destination_id).await? {
            let now = ctx.clock().clone().now(ctx.conn()).await?;
            let at = (due > now).then_some(due);
            ensure_forward_job(ctx.conn(), workspace, destination_id, at).await?;
        }
        Ok(())
    }
}

async fn claim(
    pool: &PgPool,
    workspace: WorkspaceId,
    destination: Uuid,
) -> Result<Vec<ClaimedRow>, JobError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let rows = claim_due(&mut tx, workspace, destination, BATCH, LEASE).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(rows)
}

/// Delivers one claimed row and writes how it went.
async fn forward_one(
    ctx: &mut JobContext<'_>,
    pool: &PgPool,
    destination: &Destination,
    driver: Option<&'static dyn ErasedDestination>,
    credential: &mut Option<Result<Credential, Verdict>>,
    row: &ClaimedRow,
) -> Result<(), JobError> {
    let workspace = WorkspaceId::from_uuid(row.workspace_id);
    let clock = ctx.clock().clone();
    let now = clock.now(ctx.conn()).await?;
    let done = match driver {
        None => early(Verdict::Terminal {
            status: None,
            reason: "destination_unsupported",
        }),
        Some(driver) => {
            if driver.spec().capabilities.rate_limit {
                let gates = std::sync::Arc::clone(ctx.rate_gates());
                let wait = match gates.admit(pool, destination.id, &clock).await? {
                    Admission::Granted => None,
                    Admission::Wait(wait) => Some(wait),
                    Admission::Gone => Some(GONE_WAIT),
                };
                if let Some(wait) = wait {
                    let mut tx = zirv_flow_store::begin(pool).await?;
                    defer_row(&mut tx, row.id, row.claim_token, now + wait).await?;
                    zirv_flow_store::commit(tx).await?;
                    return Ok(());
                }
            }
            if credential.is_none() {
                *credential =
                    Some(open_credential(ctx, workspace, destination, Usage::Forward).await?);
            }
            match credential.as_ref() {
                Some(Ok(credential)) => {
                    // The claim leases the whole batch for one period, and rows are delivered one
                    // at a time. Lease this row afresh right before its call, so a slow
                    // destination does not let another job generation claim and send it again. A
                    // row that was claimed again or re-driven is not ours.
                    let mut tx = zirv_flow_store::begin(pool).await?;
                    let held = extend_lease(&mut tx, row.id, row.claim_token, LEASE).await?;
                    zirv_flow_store::commit(tx).await?;
                    if !held {
                        return Ok(());
                    }
                    call(ctx, destination, driver, credential, row, now).await?
                }
                Some(Err(verdict)) => early(verdict.clone()),
                _ => early(Verdict::Terminal {
                    status: None,
                    reason: "destination_auth",
                }),
            }
        }
    };

    let target = AttemptTarget {
        destination_id: destination.id,
        subject_kind: SubjectKind::Forwarding,
        subject_id: row.id,
    };
    delivery::settle(ctx, Some(&target), row.attempt_number(), &done).await?;
    if let Verdict::Retry {
        reason: "http_429",
        retry_after,
        ..
    } = &done.verdict
        && driver.is_some_and(|driver| driver.spec().capabilities.rate_limit)
        && throttle_bucket(pool, destination.id, *retry_after, ctx.clock())
            .await
            .is_err()
    {
        warn!(destination = %destination.id, "a destination's rate could not be slowed");
    }

    let planned = plan(&done.verdict, row.attempts, now, random_unit());
    let (how, error, decision) = match &planned {
        Plan::Delivered => (Settled::Delivered, None, None),
        Plan::Retry { at, reason } => (
            Settled::Retry(*at),
            Some(*reason),
            Some((&decision_actions::EVENT_FORWARD_RETRY, *reason)),
        ),
        Plan::Dead { reason } => (
            Settled::Dead,
            Some(*reason),
            Some((&decision_actions::EVENT_FORWARD_DEAD, *reason)),
        ),
    };
    // The row and its decision commit together.
    let mut tx = zirv_flow_store::begin(pool).await?;
    let changed = settle_row(&mut tx, row.id, row.claim_token, how, error).await?;
    if let (true, Some((spec, reason))) = (changed, decision) {
        append_decision(
            &mut *tx,
            &NewDecision {
                graph: None,
                workspace_id: workspace,
                trace: ctx.trace().clone(),
                spec,
                detail: Some(reason),
                subject_ids: vec![
                    ("destination", destination.id),
                    ("rule", row.rule_id),
                    ("event", row.event_id),
                    ("outbox", row.id),
                ],
                job: Some((ctx.job_id(), ctx.attempts())),
            },
        )
        .await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

fn early(verdict: Verdict) -> Attempt {
    Attempt {
        verdict,
        elapsed: Duration::ZERO,
        may_have_arrived: false,
    }
}

/// The call to the destination: the mapped payload as the exact body text, under the row's
/// idempotency key. No redirects are followed.
async fn call(
    ctx: &JobContext<'_>,
    destination: &Destination,
    driver: &'static dyn ErasedDestination,
    credential: &Credential,
    row: &ClaimedRow,
    now: OffsetDateTime,
) -> Result<Attempt, JobError> {
    let limits = HttpLimits {
        max_redirects: 0,
        ..HttpLimits::default()
    };
    let context = DestinationContext::new(
        ctx.egress().clone(),
        limits,
        driver.spec().kind,
        destination.id.to_string(),
    )
    .map_err(|_| JobError::new("the outbound client could not be built"))?;
    let body = serde_json::to_string(&row.payload)
        .map_err(|_| JobError::new("the payload is not text"))?;
    let envelope = json!({
        "body": body,
        "timestamp": now.unix_timestamp(),
        "headers": Value::Object(Map::new()),
    });
    let outbound = Outbound {
        idempotency_key: &row.idempotency_key,
        channel: CHANNEL,
        to: "",
        payload: &envelope,
        dkim: None,
    };
    Ok(delivery::send(
        driver,
        &context,
        &destination.config,
        credential,
        &outbound,
        now,
    )
    .await)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2031-05-06 07:08:09 UTC);

    fn retry(reason: &'static str, retry_after: Option<Duration>) -> Verdict {
        Verdict::Retry {
            status: Some(503),
            reason,
            message: "m",
            retry_after,
        }
    }

    #[test]
    fn a_success_delivers_and_a_terminal_verdict_is_dead_at_once() {
        assert_eq!(
            plan(&Verdict::Succeeded { status: 200 }, 1, NOW, 0.5),
            Plan::Delivered
        );
        let terminal = Verdict::Terminal {
            status: Some(404),
            reason: "http_4xx",
        };
        assert_eq!(
            plan(&terminal, 1, NOW, 0.5),
            Plan::Dead { reason: "http_4xx" }
        );
    }

    #[test]
    fn a_retryable_verdict_backs_off_with_each_attempt_and_dies_on_the_last() {
        let waits: Vec<Duration> = (1..MAX_ATTEMPTS)
            .map(
                |attempts| match plan(&retry("http_5xx", None), attempts, NOW, 1.0) {
                    Plan::Retry { at, reason } => {
                        assert_eq!(reason, "http_5xx");
                        Duration::try_from(at - NOW).unwrap()
                    }
                    other => panic!("{other:?}"),
                },
            )
            .collect();

        assert!(waits.windows(2).all(|pair| pair[0] < pair[1]), "{waits:?}");
        assert_eq!(waits[0], Duration::from_secs(10));
        assert_eq!(
            plan(&retry("http_5xx", None), MAX_ATTEMPTS, NOW, 1.0),
            Plan::Dead { reason: "http_5xx" }
        );
    }

    #[test]
    fn a_longer_retry_after_wins_but_never_past_the_cap() {
        let at = |asked| match plan(&retry("http_429", Some(asked)), 1, NOW, 1.0) {
            Plan::Retry { at, .. } => Duration::try_from(at - NOW).unwrap(),
            other => panic!("{other:?}"),
        };

        assert_eq!(at(Duration::from_secs(120)), Duration::from_secs(120));
        assert_eq!(at(Duration::from_secs(1)), Duration::from_secs(10));
        assert_eq!(at(Duration::from_secs(86_400)), BACKOFF_CAP);
    }

    // The tests below run the job by hand against the fake destination, which keeps a ledger.

    use std::sync::Arc;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_connectors::destinations::{fake_ledger_calls, fake_ledger_sent};
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::forwarding::idempotency_key;
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{
        ClaimedJob, EventRecord, NewDestination, NewRule, Redrive, RuleWrite, apply_event_batch,
        create_destination, create_rule, redrive,
    };

    use crate::context::IngestSettings;
    use crate::delivery::rate::RateGates;

    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        destination: Uuid,
        ledger: String,
        gates: Arc<RateGates>,
        messages: std::cell::Cell<u32>,
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_private_webhook_is_dead_lettered_with_a_fixed_code(pool: PgPool) {
        use zirv_flow_core::{Keyring, SecretId, SecretPurpose, seal, secret_aad};

        let env = Env::with(
            pool,
            "private-webhook",
            "webhook",
            json!({"url": "http://169.254.169.254/secret-path?token=secret-query"}),
            100,
            100,
        )
        .await;
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let secret = SecretId::new();
        let purpose = SecretPurpose::ConnectorCredential;
        let sealed = seal(
            &keys,
            br#"{"signing_secret":"secret"}"#,
            &secret_aad(env.ws, secret, purpose),
        );
        zirv_flow_store::secrets::insert_unchecked(&env.pool, env.ws, purpose, secret, &sealed)
            .await
            .unwrap();
        let credential = zirv_flow_store::import_secret_as_credential(
            &env.pool,
            env.ws,
            "Signing",
            secret,
            &["signing_secret"],
        )
        .await
        .unwrap();
        sqlx::query("UPDATE destinations SET credential_id = $2 WHERE id = $1")
            .bind(env.destination)
            .bind(credential.id)
            .execute(&env.pool)
            .await
            .unwrap();
        env.order().await;
        env.execute_with_keys(env.claim().await.unwrap(), Some(keys))
            .await
            .unwrap();
        assert_eq!(
            env.rows().await,
            [("dead".to_owned(), 1, Some("egress_denied".to_owned()))]
        );
        assert!(env.claim().await.is_none(), "the refusal is terminal");
    }

    impl Env {
        async fn new(pool: PgPool, name: &str, mode: &str) -> Self {
            Self::with(pool, name, "fake", json!({"mode": mode}), 100, 100).await
        }

        async fn with(
            pool: PgPool,
            name: &str,
            kind: &str,
            config: Value,
            rate: i32,
            burst: i32,
        ) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse(name).unwrap(),
                &Slug::parse(name).unwrap(),
            )
            .await
            .unwrap()
            .id;
            let ledger = format!("forward-{name}-{}", Uuid::now_v7());
            let mut config = config;
            config["ledger"] = json!(ledger);
            let destination = create_destination(
                &pool,
                ws,
                &NewDestination {
                    id: Uuid::now_v7(),
                    kind,
                    name: "Sink",
                    config: &config,
                    credential_id: None,
                    rate,
                    burst,
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .id;
            let mut connection = pool.acquire().await.unwrap();
            let rule = create_rule(
                &mut connection,
                ws,
                &NewRule {
                    id: Uuid::now_v7(),
                    destination_id: destination,
                    name: "Orders",
                    event_name: "order",
                    filter: &json!([]),
                    mapping: &json!({"name": {"from": "event.name"},
                                     "plan": {"from": "event.properties", "pointer": "/plan"}}),
                    active: true,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(rule, RuleWrite::Saved(_)));
            drop(connection);
            Self {
                pool,
                ws,
                destination,
                ledger,
                gates: Arc::default(),
                messages: std::cell::Cell::new(0),
            }
        }

        /// Accepts one `order` event, which the rule copies to the outbox; its id.
        async fn order(&self) -> Uuid {
            let n = self.messages.get();
            self.messages.set(n + 1);
            let key = HmacRoot::bootstrap_from(
                &MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap(),
            )
            .hmac_key(HmacPurpose::Redaction);
            let outcomes = apply_event_batch(
                &self.pool,
                self.ws,
                &key,
                &[EventRecord {
                    source_record_id: None,
                    source: "web".into(),
                    identifiers: vec![("email".into(), "ada@example.com".into())],
                    name: "order".into(),
                    properties: json!({"plan": "pro"}).as_object().cloned().unwrap(),
                    timestamp: None,
                    received_at: OffsetDateTime::now_utc(),
                    message_id: Some(format!("m-{n}")),
                    trace: TraceContext::root(Cause::api("t")),
                }],
            )
            .await
            .unwrap();
            match &outcomes[0] {
                zirv_flow_store::EventOutcome::Accepted { event_id, .. } => *event_id,
                other => panic!("{other:?}"),
            }
        }

        async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[FORWARD_JOB_KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
        }

        /// Runs one claimed job as a worker does: its work commits with its completion.
        async fn execute(&self, claimed: ClaimedJob) -> Result<(), JobError> {
            self.execute_with_keys(claimed, None).await
        }

        async fn execute_with_keys(
            &self,
            claimed: ClaimedJob,
            keys: Option<Arc<zirv_flow_core::Keyring>>,
        ) -> Result<(), JobError> {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let result = {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_ingest(IngestSettings {
                        pool: self.pool.clone(),
                        redaction_key: None,
                        batch_size: 1,
                        shards: 1,
                    })
                    .with_rate_gates(Arc::clone(&self.gates))
                    .with_outbound(
                        keys,
                        Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap()),
                        Arc::default(),
                    );
                EventForward.run(&mut ctx, claimed.payload.clone()).await
            };
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
                    Err(error)
                }
            }
        }

        /// Makes every waiting row and job due now, claims the job and runs it.
        async fn run(&self) {
            sqlx::query(
                "UPDATE forwarding_outbox SET next_attempt_at = now() WHERE state = 'pending'",
            )
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'event.forward'")
                .execute(&self.pool)
                .await
                .unwrap();
            let claimed = self.claim().await.expect("an event.forward job is ready");
            self.execute(claimed).await.unwrap();
        }

        async fn set_mode(&self, mode: &str) {
            sqlx::query("UPDATE destinations SET config = jsonb_set(config, '{mode}', to_jsonb($2::text)) WHERE id = $1")
                .bind(self.destination)
                .bind(mode)
                .execute(&self.pool)
                .await
                .unwrap();
        }

        async fn rows(&self) -> Vec<(String, i32, Option<String>)> {
            sqlx::query_as(
                "SELECT state, attempts, last_error FROM forwarding_outbox ORDER BY created_at, id",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn attempts(&self) -> Vec<(i32, String)> {
            sqlx::query_as(
                "SELECT attempt, outcome FROM delivery_attempts \
                 WHERE subject_kind = 'forwarding' ORDER BY attempt",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn decisions(&self) -> Vec<(String, Option<String>)> {
            sqlx::query_as(
                "SELECT outcome, reason FROM decision_records \
                 WHERE workspace_id = $1 AND action = 'event.forward' ORDER BY recorded_at, id",
            )
            .bind(self.ws.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_purge_commits_its_rows_and_markers_before_the_job_transaction_ends(pool: PgPool) {
        let env = Env::new(pool, "purge", "ok").await;
        env.order().await;
        sqlx::query(
            "UPDATE forwarding_outbox SET state = 'delivered', payload = '{}',
             updated_at = now() - interval '8 days'",
        )
        .execute(&env.pool)
        .await
        .unwrap();
        let claimed = env.claim().await.unwrap();
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(IngestSettings {
            pool: env.pool.clone(),
            redaction_key: None,
            batch_size: 1,
            shards: 1,
        });

        EventForward
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();

        assert_eq!(env.count("SELECT count(*) FROM forwarding_outbox").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM erasure_handled_markers")
                .await,
            1
        );
        tx.rollback().await.unwrap();
        assert_eq!(env.count("SELECT count(*) FROM forwarding_outbox").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mapped_event_is_delivered_under_its_stable_key_and_the_attempt_is_recorded(
        pool: PgPool,
    ) {
        let env = Env::new(pool, "deliver", "ok").await;
        let event = env.order().await;
        let events_before = env.count("SELECT count(*) FROM events").await;

        env.run().await;

        let key = idempotency_key(event, env.destination);
        let sent = fake_ledger_sent(&env.ledger);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].key, key);
        let body: Value = serde_json::from_str(sent[0].payload["body"].as_str().unwrap()).unwrap();
        assert_eq!(body, json!({"name": "order", "plan": "pro"}));
        assert_eq!(env.rows().await, [("delivered".to_owned(), 1, None)]);
        let payload: Value = sqlx::query_scalar("SELECT payload FROM forwarding_outbox")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(payload, json!({}), "a delivered row keeps no payload");
        assert_eq!(env.attempts().await, [(1, "delivered".to_owned())]);
        assert!(
            env.decisions().await.is_empty(),
            "a delivery is not a decision"
        );
        // The job writes no event and starts no flow.
        assert_eq!(
            env.count("SELECT count(*) FROM events").await,
            events_before
        );
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind LIKE 'flow.%'")
                .await,
            0
        );
        assert_eq!(env.count("SELECT count(*) FROM enrollments").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'event.forward' AND state = 'ready'")
                .await,
            0,
            "nothing is left to deliver"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retry_reuses_the_key_backs_off_and_leaves_a_job_for_the_next_attempt(pool: PgPool) {
        let env = Env::new(pool, "retry", "lose_first_answer").await;
        let event = env.order().await;
        let key = idempotency_key(event, env.destination);

        env.run().await;

        assert_eq!(
            env.rows().await,
            [("pending".to_owned(), 1, Some("http_5xx".to_owned()))]
        );
        let wait: f64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM next_attempt_at - now())::float8 FROM forwarding_outbox",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(
            (1.0..=10.5).contains(&wait),
            "the first backoff is about 5 s: {wait}"
        );
        assert_eq!(
            env.decisions().await,
            [("retry".to_owned(), Some("http_5xx".to_owned()))]
        );
        let successor: (String, bool) =
            sqlx::query_as("SELECT state, run_at > now() FROM jobs WHERE kind = 'event.forward'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            successor,
            ("ready".to_owned(), true),
            "the next attempt waits for its time"
        );

        env.run().await;

        assert_eq!(fake_ledger_calls(&env.ledger), [key.clone(), key.clone()]);
        assert_eq!(fake_ledger_sent(&env.ledger).len(), 1);
        assert_eq!(env.rows().await, [("delivered".to_owned(), 2, None)]);
        assert_eq!(
            env.attempts().await,
            [(1, "retryable".to_owned()), (2, "delivered".to_owned())]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn after_the_last_attempt_the_row_is_a_dead_letter_with_a_decision_for_each_step(
        pool: PgPool,
    ) {
        let env = Env::new(pool, "dead", "server_error").await;
        env.order().await;

        for _ in 0..MAX_ATTEMPTS {
            env.run().await;
        }

        assert_eq!(
            env.rows().await,
            [("dead".to_owned(), MAX_ATTEMPTS, Some("http_5xx".to_owned()))]
        );
        let decisions = env.decisions().await;
        assert_eq!(decisions.len(), MAX_ATTEMPTS as usize);
        assert_eq!(decisions.last().unwrap().0, "dead");
        assert!(
            decisions[..decisions.len() - 1]
                .iter()
                .all(|(outcome, _)| outcome == "retry")
        );
        assert_eq!(env.attempts().await.len(), MAX_ATTEMPTS as usize);
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'event.forward' AND state = 'ready'")
                .await,
            0,
            "a dead letter schedules nothing"
        );
        // Nothing of the payload reached a decision.
        let text: Vec<String> =
            sqlx::query_scalar("SELECT subject_ids::text FROM decision_records")
                .fetch_all(&env.pool)
                .await
                .unwrap();
        assert!(text.iter().all(|row| !row.contains("pro")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_terminal_answer_is_dead_at_once_and_a_redrive_delivers_it_without_a_flow(
        pool: PgPool,
    ) {
        let env = Env::new(pool, "redrive", "rejected").await;
        let event = env.order().await;
        let events_before = env.count("SELECT count(*) FROM events").await;
        env.run().await;
        assert_eq!(
            env.rows().await,
            [("dead".to_owned(), 1, Some("http_4xx".to_owned()))]
        );
        assert_eq!(
            env.decisions().await,
            [("dead".to_owned(), Some("http_4xx".to_owned()))]
        );

        env.set_mode("ok").await;
        let mut connection = env.pool.acquire().await.unwrap();
        let redriven = redrive(
            &mut connection,
            env.ws,
            Redrive::Destination(env.destination),
        )
        .await
        .unwrap();
        drop(connection);
        assert_eq!(redriven, 1);
        env.run().await;

        assert_eq!(env.rows().await, [("delivered".to_owned(), 1, None)]);
        assert_eq!(
            env.attempts().await,
            [(1, "terminal".to_owned()), (2, "delivered".to_owned())],
            "the attempt numbers continue past the re-drive"
        );
        assert_eq!(
            fake_ledger_sent(&env.ledger)[0].key,
            idempotency_key(event, env.destination)
        );
        assert_eq!(
            env.count("SELECT count(*) FROM events").await,
            events_before
        );
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind LIKE 'flow.%'")
                .await,
            0
        );
        assert_eq!(env.count("SELECT count(*) FROM enrollments").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rate_limit_wait_gives_the_row_back_without_spending_an_attempt(pool: PgPool) {
        let env = Env::with(pool, "rate", "fake", json!({"mode": "ok"}), 1, 1).await;
        env.order().await;
        env.order().await;

        env.run().await;

        let rows = env.rows().await;
        let delivered = rows.iter().filter(|row| row.0 == "delivered").count();
        assert_eq!(delivered, 1, "{rows:?}");
        assert!(
            rows.contains(&("pending".to_owned(), 0, None)),
            "the second row waits for a token and spends no attempt: {rows:?}"
        );
        assert_eq!(env.attempts().await.len(), 1);
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1);
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'event.forward' AND state = 'ready'")
                .await,
            1,
            "a job is left for the waiting row"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_destination_that_cannot_carry_events_kills_its_rows_with_a_fixed_code(pool: PgPool) {
        let env = Env::with(pool, "mail", "smtp", json!({}), 10, 10).await;
        env.order().await;

        env.run().await;

        assert_eq!(
            env.rows().await,
            [(
                "dead".to_owned(),
                1,
                Some("destination_unsupported".to_owned())
            )]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_destination_is_left_alone_and_a_payload_for_another_workspace_fails(
        pool: PgPool,
    ) {
        let env = Env::new(pool, "archived", "ok").await;
        env.order().await;
        sqlx::query("UPDATE destinations SET archived_at = now() WHERE id = $1")
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();

        env.run().await;

        assert_eq!(env.rows().await, [("pending".to_owned(), 0, None)]);
        assert!(fake_ledger_calls(&env.ledger).is_empty());

        let mut connection = env.pool.acquire().await.unwrap();
        ensure_forward_job(&mut connection, env.ws, env.destination, None)
            .await
            .unwrap();
        drop(connection);
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'event.forward'")
            .execute(&env.pool)
            .await
            .unwrap();
        let claimed = env.claim().await.expect("a job is ready");
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);
        let refused = EventForward
            .run(
                &mut ctx,
                json!({"workspace_id": WorkspaceId::new().to_string(),
                       "destination_id": env.destination.to_string()}),
            )
            .await;
        assert!(refused.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_whose_claim_was_taken_over_is_skipped_and_no_call_is_made(pool: PgPool) {
        let env = Env::new(pool, "takeover", "ok").await;
        env.order().await;
        let mut connection = env.pool.acquire().await.unwrap();
        let first = zirv_flow_store::claim_due(
            &mut connection,
            env.ws,
            env.destination,
            BATCH,
            Duration::from_secs(300),
        )
        .await
        .unwrap()
        .remove(0);
        // The lease ran out and another job generation claimed the row.
        sqlx::query("UPDATE forwarding_outbox SET next_attempt_at = now()")
            .execute(&mut *connection)
            .await
            .unwrap();
        let second = zirv_flow_store::claim_due(
            &mut connection,
            env.ws,
            env.destination,
            BATCH,
            Duration::from_secs(300),
        )
        .await
        .unwrap()
        .remove(0);
        drop(connection);
        assert_ne!(first.claim_token, second.claim_token);

        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'event.forward'")
            .execute(&env.pool)
            .await
            .unwrap();
        let claimed = env.claim().await.expect("an event.forward job is ready");
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_rate_gates(Arc::clone(&env.gates))
            .with_outbound(
                None,
                Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap()),
                Arc::default(),
            );
        let destination = find_destination(ctx.conn(), env.ws, env.destination)
            .await
            .unwrap()
            .unwrap();
        let driver = Registry::built_in().get(&destination.kind);
        let mut credential = None;

        forward_one(
            &mut ctx,
            &env.pool,
            &destination,
            driver,
            &mut credential,
            &first,
        )
        .await
        .unwrap();
        drop(ctx);
        drop(tx);

        assert!(fake_ledger_calls(&env.ledger).is_empty(), "no call is made");
        assert!(env.attempts().await.is_empty());
        assert_eq!(env.rows().await, [("pending".to_owned(), 2, None)]);
        let token: Option<Uuid> = sqlx::query_scalar("SELECT claim_token FROM forwarding_outbox")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(token, Some(second.claim_token), "the newer claim is intact");
    }
}
