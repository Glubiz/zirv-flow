//! The delivery code every outbound job shares (architecture 10.1 and 14): one verdict function
//! that says what a destination's answer means, and one write of the `delivery_attempts` row.
//! `webhook.call` uses it now and `delivery.send` and the other destination jobs use it as they
//! arrive, so a retry rule or a row shape is changed in one place.
//!
//! - [`send`] calls a destination and judges the result into a [`Verdict`].
//! - [`rate::RateGates`] holds the process's view of each destination's rate bucket.
//! - [`settle`] records the attempt for a destination that has a row, in a transaction of its own,
//!   and counts it after that commits. A call with no destination row, such as the raw URL of an
//!   `action.webhook` node, records nothing and touches no rate bucket.
//!
//! A handler must not hold a row lock on its destination (`FOR UPDATE`) in the job transaction
//! across [`settle`]: the attempt insert runs on a second connection and needs a share lock on that
//! row for the foreign key, so it would wait on a lock its own job holds.

pub(crate) mod attempt;
pub(crate) mod credential;
pub(crate) mod rate;
pub(crate) mod verdict;

pub(crate) use attempt::settle;
pub(crate) use verdict::{Verdict, arrived, verdict_of};

use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use zirv_flow_connectors::destinations::{
    DestinationContext, ErasedDestination, Outbound, ProfileCall,
};
use zirv_flow_connectors::sources::Credential;

/// One call to a destination and how it went.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Attempt {
    pub(crate) verdict: Verdict,
    /// How long the call took.
    pub(crate) elapsed: Duration,
    /// Whether the destination may have received the call, so it may have acted on it even though
    /// the verdict is a failure (a timeout or reset after the request was written). Only
    /// `delivery.send` reads it.
    pub(crate) may_have_arrived: bool,
}

/// Delivers `message` through `destination` and judges the answer at `now`.
pub(crate) async fn send(
    destination: &dyn ErasedDestination,
    ctx: &DestinationContext,
    config: &Value,
    credential: &Credential,
    message: &Outbound<'_>,
    now: OffsetDateTime,
) -> Attempt {
    let started = Instant::now();
    let result = destination.deliver(ctx, config, credential, message).await;
    Attempt {
        verdict: verdict_of(&result, now),
        elapsed: started.elapsed(),
        may_have_arrived: arrived(&result),
    }
}

/// Runs one profile operation through `destination` and judges the answer at `now`, as [`send`]
/// does for a message.
pub(crate) async fn sync(
    destination: &dyn ErasedDestination,
    ctx: &DestinationContext,
    config: &Value,
    credential: &Credential,
    call: &ProfileCall<'_>,
    now: OffsetDateTime,
) -> Attempt {
    let started = Instant::now();
    let result = destination.sync(ctx, config, credential, call).await;
    Attempt {
        verdict: verdict_of(&result, now),
        elapsed: started.elapsed(),
        may_have_arrived: arrived(&result),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_connectors::destinations::Registry;
    use zirv_flow_connectors::http::HttpLimits;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{AttemptOutcome, NewDeliveryAttempt, NewJob, SubjectKind};

    use super::attempt::AttemptTarget;
    use super::*;
    use crate::JobContext;

    #[tokio::test]
    async fn a_destination_is_called_and_its_answer_judged() {
        let registry = Registry::built_in();
        let fake = registry.get("fake").expect("the fake is registered");
        let ctx = DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "fake",
            "destination-1",
        )
        .unwrap();
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "key-1",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH;

        for (mode, expected) in [
            ("ok", Verdict::Succeeded { status: 202 }),
            (
                "rejected",
                Verdict::Terminal {
                    status: Some(422),
                    reason: "http_4xx",
                },
            ),
        ] {
            let attempt = send(
                fake,
                &ctx,
                &json!({"mode": mode}),
                &Credential::new(),
                &message,
                now,
            )
            .await;

            assert_eq!(attempt.verdict, expected, "{mode}");
        }
    }

    /// What `delivery.send` (#220) does with a destination row: send, then settle. The job in
    /// `kinds/webhook_call.rs` makes the same two calls, with no row for a raw URL.
    async fn fixture_delivery_send(
        ctx: &mut JobContext<'_>,
        target: &AttemptTarget,
        number: i32,
        kind: &str,
        config: &Value,
        credential: &Credential,
    ) -> NewDeliveryAttempt {
        let registry = Registry::built_in();
        let destination = registry.get(kind).expect("the kind is registered");
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        let limits = HttpLimits {
            max_redirects: 0,
            ..HttpLimits::default()
        };
        let client = DestinationContext::new(Arc::new(policy), limits, "test", "d").unwrap();
        let payload = json!({"body": "{}", "timestamp": 1_700_000_000});
        let message = Outbound {
            idempotency_key: "key-1",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };
        let done = send(
            destination,
            &client,
            config,
            credential,
            &message,
            OffsetDateTime::UNIX_EPOCH,
        )
        .await;
        settle(ctx, Some(target), number, &done)
            .await
            .unwrap()
            .expect("a destination row records the attempt")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_webhook_and_a_fixture_send_share_the_verdict_function_and_the_attempt_row(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("shared").unwrap();
        let slug = Slug::parse("shared").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let app = Router::new().route("/hook", post(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut targets = Vec::new();
        for (kind, label, config) in [
            ("webhook", "hook", json!({"url": url})),
            ("fake", "fixture", json!({"mode": "server_error"})),
        ] {
            let id = Uuid::now_v7();
            let created = zirv_flow_store::create_destination(
                &pool,
                ws,
                &zirv_flow_store::NewDestination {
                    id,
                    kind,
                    name: label,
                    config: &config,
                    credential_id: None,
                    rate: 10,
                    burst: 10,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            targets.push((kind, created.id, config));
        }
        let mut job = NewJob::new(
            "test.delivery",
            TraceContext::root(Cause::api("test-request")),
        );
        job.workspace_id = Some(ws);
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &["test.delivery"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&pool));
        let mut credential = Credential::new();
        credential.insert(
            "signing_secret",
            zirv_flow_core::SecretString::new("zfwh_x".into()),
        );

        let mut rows = Vec::new();
        for (kind, destination_id, config) in &targets {
            let target = AttemptTarget {
                destination_id: *destination_id,
                subject_kind: SubjectKind::Message,
                subject_id: Uuid::from_u128(5),
            };
            rows.push(fixture_delivery_send(&mut ctx, &target, 1, kind, config, &credential).await);
        }
        zirv_flow_store::commit(tx).await.unwrap();

        let stored: Vec<(String, String, String, i32)> = sqlx::query_as(
            "SELECT outcome, status_class, subject_kind, attempt FROM delivery_attempts \n             ORDER BY destination_id = $1 DESC",
        )
        .bind(targets[0].1)
        .fetch_all(&pool)
        .await
        .unwrap();
        let want = (
            "retryable".to_owned(),
            "5xx".to_owned(),
            "message".to_owned(),
            1,
        );
        assert_eq!(
            stored,
            [want.clone(), want],
            "one row shape for both destinations"
        );
        assert_eq!(rows[0].outcome, rows[1].outcome);
        assert_eq!(rows[0].status_class, rows[1].status_class);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_with_no_destination_row_records_and_counts_nothing(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("raw").unwrap();
        let slug = Slug::parse("raw").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let mut job = NewJob::new(
            "test.delivery",
            TraceContext::root(Cause::api("test-request")),
        );
        job.workspace_id = Some(ws);
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &["test.delivery"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&pool));
        let done = Attempt {
            verdict: Verdict::Succeeded { status: 200 },
            elapsed: Duration::ZERO,
            may_have_arrived: false,
        };

        let recorded = settle(&mut ctx, None, 1, &done).await.unwrap();

        assert!(recorded.is_none());
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_attempts")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    fn worker_pool(pool: &PgPool) -> crate::context::IngestSettings {
        crate::context::IngestSettings {
            pool: pool.clone(),
            redaction_key: None,
            batch_size: 1,
            shards: 1,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_attempt_row_survives_the_rollback_of_a_job_that_retries(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("retry").unwrap();
        let slug = Slug::parse("retry").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let app = Router::new().route("/hook", post(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = json!({"url": url});
        let destination = zirv_flow_store::create_destination(
            &pool,
            ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "webhook",
                name: "hook",
                config: &config,
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let mut job = NewJob::new(
            "test.delivery",
            TraceContext::root(Cause::api("test-request")),
        );
        job.workspace_id = Some(ws);
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &["test.delivery"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut credential = Credential::new();
        credential.insert(
            "signing_secret",
            zirv_flow_core::SecretString::new("zfwh_x".into()),
        );
        let target = AttemptTarget {
            destination_id: destination.id,
            subject_kind: SubjectKind::Message,
            subject_id: Uuid::from_u128(5),
        };

        // The first attempt is retried: the job returns an error and its transaction rolls back.
        {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&pool));
            let row =
                fixture_delivery_send(&mut ctx, &target, 1, "webhook", &config, &credential).await;
            assert_eq!(row.outcome, AttemptOutcome::Retryable);
            drop(tx);
        }
        let after_retry: Vec<(String, i32)> =
            sqlx::query_as("SELECT outcome, attempt FROM delivery_attempts ORDER BY attempt")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(after_retry, [("retryable".to_owned(), 1)]);

        // The second attempt succeeds and commits.
        let second = target;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&pool));
        let done = Attempt {
            verdict: Verdict::Succeeded { status: 200 },
            elapsed: Duration::ZERO,
            may_have_arrived: false,
        };
        settle(&mut ctx, Some(&second), 2, &done).await.unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let all: Vec<(String, i32)> =
            sqlx::query_as("SELECT outcome, attempt FROM delivery_attempts ORDER BY attempt")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            all,
            [("retryable".to_owned(), 1), ("delivered".to_owned(), 2)]
        );
    }

    /// A workspace, a fake destination row and a claimed job of the workspace.
    async fn recording_fixture(
        pool: &PgPool,
        name: &str,
    ) -> (
        zirv_flow_core::WorkspaceId,
        Uuid,
        zirv_flow_store::ClaimedJob,
    ) {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            pool,
            &WorkspaceName::parse(name).unwrap(),
            &Slug::parse(name).unwrap(),
        )
        .await
        .unwrap()
        .id;
        let config = json!({"mode": "ok"});
        let destination = zirv_flow_store::create_destination(
            pool,
            ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "fixture",
                config: &config,
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let mut job = NewJob::new(
            "test.delivery",
            TraceContext::root(Cause::api("test-request")),
        );
        job.workspace_id = Some(ws);
        zirv_flow_store::enqueue(pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &["test.delivery"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        (ws, destination.id, claimed)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_that_succeeds_stays_a_success_when_recording_it_fails(pool: PgPool) {
        let metrics = metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .unwrap();
        let (ws, destination, claimed) = recording_fixture(&pool, "norecord").await;
        let broken = PgPool::connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        broken.close().await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&broken));
        let target = AttemptTarget {
            destination_id: destination,
            subject_kind: SubjectKind::Message,
            subject_id: Uuid::from_u128(5),
        };

        let row = fixture_delivery_send(
            &mut ctx,
            &target,
            1,
            "fake",
            &json!({"mode": "ok"}),
            &Credential::new(),
        )
        .await;

        assert_eq!(row.outcome, AttemptOutcome::Delivered);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        let output = metrics.render();
        let want = format!(
            "zirv_flow_delivery_attempt_record_failures_total{{workspace=\"{}\",destination=\"{destination}\"}} 1",
            ws.as_uuid()
        );
        assert!(output.contains(&want), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_row_is_written_even_when_the_handler_is_dropped_while_recording(pool: PgPool) {
        use futures_util::FutureExt;

        let (_, destination, claimed) = recording_fixture(&pool, "cancelled").await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(worker_pool(&pool));
        let target = AttemptTarget {
            destination_id: destination,
            subject_kind: SubjectKind::Message,
            subject_id: Uuid::from_u128(5),
        };
        let done = Attempt {
            verdict: Verdict::Succeeded { status: 200 },
            elapsed: Duration::ZERO,
            may_have_arrived: false,
        };

        // One poll starts the recording and waits for it; the future is dropped there, as the
        // worker drops a handler whose lease was lost.
        let polled = settle(&mut ctx, Some(&target), 1, &done).now_or_never();
        assert!(polled.is_none(), "settle was waiting on the recording");

        let mut rows = 0_i64;
        for _ in 0..200 {
            rows = sqlx::query_scalar("SELECT count(*) FROM delivery_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
            if rows == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(rows, 1, "the spawned recording finished after the drop");
    }
}
