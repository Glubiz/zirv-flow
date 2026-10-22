//! The delivery code every outbound job shares (architecture 10.1 and 14): one verdict function
//! that says what a destination's answer means, and one write of the `delivery_attempts` row.
//! `webhook.call` uses it now and `delivery.send` and the other destination jobs use it as they
//! arrive, so a retry rule or a row shape is changed in one place.
//!
//! - [`send`] calls a destination and judges the result into a [`Verdict`].
//! - [`settle`] records the attempt for a destination that has a row and counts it once the job
//!   transaction has committed. A call with no destination row, such as the raw URL of an
//!   `action.webhook` node, records nothing and touches no rate bucket.

pub(crate) mod attempt;
pub(crate) mod verdict;

pub(crate) use attempt::settle;
pub(crate) use verdict::{Verdict, verdict_of};

use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use zirv_flow_connectors::destinations::{DestinationContext, ErasedDestination, Outbound};
use zirv_flow_connectors::sources::Credential;

/// One call to a destination and how it went.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Attempt {
    pub(crate) verdict: Verdict,
    /// How long the call took.
    pub(crate) elapsed: Duration,
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
    use zirv_flow_store::{NewDeliveryAttempt, NewJob, SubjectKind};

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
        settle(ctx, Some(target), 1, &done)
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
                    credential_secret_id: None,
                    credential_fields: &[],
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
        let mut ctx = JobContext::new(&mut tx, &claimed);
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
            rows.push(fixture_delivery_send(&mut ctx, &target, kind, config, &credential).await);
        }
        let counted = ctx.take_after_commit().len();
        zirv_flow_store::commit(tx).await.unwrap();

        assert_eq!(counted, 2, "each attempt is counted once, after the commit");
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
        let mut ctx = JobContext::new(&mut tx, &claimed);
        let done = Attempt {
            verdict: Verdict::Succeeded { status: 200 },
            elapsed: Duration::ZERO,
        };

        let recorded = settle(&mut ctx, None, 1, &done).await.unwrap();

        assert!(recorded.is_none());
        assert!(ctx.take_after_commit().is_empty());
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_attempts")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
