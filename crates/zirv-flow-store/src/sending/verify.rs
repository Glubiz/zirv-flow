//! Recording the result of a DNS check. This is the only place a domain's status changes: it is
//! `verified` exactly when its DKIM, SPF and DMARC records are all `ok`, and a change in either
//! direction appends a `sending_domain.verify` decision.

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::sending::{RecordKind, RecordStatus};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::domains::{
    DomainRecord, SendingDomain, UNVERIFIED, VERIFIED, domain_columns, list_domain_records,
};
use crate::{NewDecision, NewJob, StoreError, append_decision, enqueue};

/// The job kind that checks a domain's DNS records.
pub const SENDING_DOMAIN_VERIFY_KIND: &str = "sending_domain.verify";

/// Queues a check of the domain's DNS records under `trace`. At most one check per domain is
/// ready or running at a time: a second request while one waits is a no-op, and returns false.
///
/// A queued check stamps the domain's `last_attempt_at`, which the leader's scan reads as the
/// start of a back-off, so a check that keeps failing is not re-queued at once.
pub async fn enqueue_domain_verify(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    domain: Uuid,
    trace: TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(SENDING_DOMAIN_VERIFY_KIND, trace);
    job.workspace_id = Some(workspace);
    job.payload = json!({"workspace_id": workspace.to_string(), "domain_id": domain.to_string()});
    job.unique_key = Some(format!("{SENDING_DOMAIN_VERIFY_KIND}:{domain}"));
    let queued = enqueue(&mut *connection, &job).await?;
    if queued {
        sqlx::query(
            "UPDATE sending_domains SET last_attempt_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(domain)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    }
    Ok(queued)
}

/// How many observed TXT records, and how many characters of each, a record keeps.
const MAX_OBSERVED: usize = 8;
const MAX_OBSERVED_CHARS: usize = 1024;

/// What a check found at one record's name. A record the check could not look up (the name
/// servers did not answer) is left out, and keeps its last status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordFinding {
    pub kind: RecordKind,
    pub status: RecordStatus,
    /// The TXT records read at the record's name.
    pub observed: Vec<String>,
}

/// The result of recording a check.
#[derive(Clone, Debug, PartialEq)]
pub struct DomainCheck {
    pub domain: SendingDomain,
    pub records: Vec<DomainRecord>,
    /// The status before the check, `unverified` or `verified`.
    pub previous_status: String,
}

impl DomainCheck {
    /// Whether the check changed the domain's status.
    pub fn changed(&self) -> bool {
        self.previous_status != self.domain.status
    }
}

fn bounded(observed: &[String]) -> Vec<String> {
    observed
        .iter()
        .take(MAX_OBSERVED)
        .map(|text| text.chars().take(MAX_OBSERVED_CHARS).collect())
        .collect()
}

/// Records `findings` on the domain, recomputes its status from all three records, stamps the
/// check time, and appends a decision under `trace` when the status changed. `None` when the
/// workspace has no such domain (it was deleted while the check ran).
///
/// The domain row is locked for the call, so two checks of one domain record one after the other.
pub async fn record_domain_check(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    domain_id: Uuid,
    findings: &[RecordFinding],
    trace: &TraceContext,
    job: Option<(Uuid, i32)>,
) -> Result<Option<DomainCheck>, StoreError> {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT status FROM sending_domains WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(domain_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(previous_status) = previous else {
        return Ok(None);
    };
    for finding in findings {
        sqlx::query(
            "UPDATE sending_domain_records SET status = $4, observed_text = $5, checked_at = now() \
             WHERE workspace_id = $1 AND domain_id = $2 AND kind = $3",
        )
        .bind(workspace.as_uuid())
        .bind(domain_id)
        .bind(finding.kind.as_str())
        .bind(finding.status.as_str())
        .bind(bounded(&finding.observed))
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    let all_ok: bool = sqlx::query_scalar(
        "SELECT count(*) = 3 FROM sending_domain_records \
         WHERE workspace_id = $1 AND domain_id = $2 AND status = 'ok'",
    )
    .bind(workspace.as_uuid())
    .bind(domain_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let status = if all_ok { VERIFIED } else { UNVERIFIED };
    let domain: SendingDomain = sqlx::query_as(concat!(
        "UPDATE sending_domains SET status = $3, \
            verified_at = CASE WHEN $3 = 'verified' AND status <> 'verified' THEN now() \
                               ELSE verified_at END, \
            last_checked_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        domain_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(domain_id)
    .bind(status)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let records = list_domain_records(&mut *connection, workspace, domain_id).await?;
    let check = DomainCheck {
        domain,
        records,
        previous_status,
    };
    if check.changed() {
        let (outcome, reason) = if all_ok {
            ("verified", "records_ok")
        } else {
            ("unverified", "record_failed")
        };
        append_decision(
            connection,
            &NewDecision {
                graph: None,
                workspace_id: workspace,
                trace: trace.clone(),
                action: "sending_domain.verify",
                outcome,
                reason: Some(reason),
                subject_ids: vec![("sending_domain", domain_id)],
                job,
            },
        )
        .await?;
    }
    Ok(Some(check))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::Cause;

    use super::*;
    use crate::list_trace;
    use crate::sending::fixtures::plant_domain;
    use crate::templates::tests_support::workspace;

    fn finding(kind: RecordKind, status: RecordStatus) -> RecordFinding {
        RecordFinding {
            kind,
            status,
            observed: vec![format!("seen {}", kind.as_str())],
        }
    }

    fn all(status: RecordStatus) -> Vec<RecordFinding> {
        RecordKind::ALL
            .into_iter()
            .map(|kind| finding(kind, status))
            .collect()
    }

    async fn check(
        pool: &PgPool,
        ws: WorkspaceId,
        domain: Uuid,
        findings: &[RecordFinding],
        trace: &TraceContext,
    ) -> DomainCheck {
        let mut connection = pool.acquire().await.unwrap();
        record_domain_check(&mut connection, ws, domain, findings, trace, None)
            .await
            .unwrap()
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_is_verified_only_when_all_three_records_are_ok(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let trace = TraceContext::root(Cause::definition_change(id));

        let two = check(
            &pool,
            ws,
            id,
            &[
                finding(RecordKind::Dkim, RecordStatus::Ok),
                finding(RecordKind::Spf, RecordStatus::Ok),
                finding(RecordKind::Dmarc, RecordStatus::Missing),
            ],
            &trace,
        )
        .await;
        assert_eq!(two.domain.status, "unverified");
        assert!(!two.changed() && two.domain.verified_at.is_none());
        assert!(two.domain.last_checked_at.is_some());

        let three = check(
            &pool,
            ws,
            id,
            &[finding(RecordKind::Dmarc, RecordStatus::Ok)],
            &trace,
        )
        .await;
        assert_eq!(three.domain.status, "verified");
        assert!(three.changed() && three.domain.verified_at.is_some());
        let statuses: Vec<_> = three.records.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(statuses, ["ok", "ok", "ok"]);
        assert_eq!(three.records[0].observed_text, ["seen dkim"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_recheck_drops_a_verified_domain_and_appends_a_decision(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let up = TraceContext::root(Cause::definition_change(id));
        let down = TraceContext::root(Cause::schedule(id));
        let verified = check(&pool, ws, id, &all(RecordStatus::Ok), &up).await;
        let stamp = verified.domain.verified_at.unwrap();

        let still = check(&pool, ws, id, &all(RecordStatus::Ok), &up).await;
        assert_eq!(still.domain.verified_at, Some(stamp));
        let dropped = check(
            &pool,
            ws,
            id,
            &[finding(RecordKind::Spf, RecordStatus::Mismatch)],
            &down,
        )
        .await;

        assert_eq!(dropped.domain.status, "unverified");
        assert!(dropped.changed());
        let (up_log, _) = list_trace(&pool, ws, up.trace_id, 10).await.unwrap();
        let (down_log, _) = list_trace(&pool, ws, down.trace_id, 10).await.unwrap();
        // Becoming verified is one decision; staying verified appends none.
        assert_eq!(up_log.len(), 1);
        assert_eq!(
            (up_log[0].action.as_str(), up_log[0].outcome.as_str()),
            ("sending_domain.verify", "verified")
        );
        assert_eq!(down_log.len(), 1);
        assert_eq!(
            (down_log[0].action.as_str(), down_log[0].outcome.as_str()),
            ("sending_domain.verify", "unverified")
        );
        assert_eq!(down_log[0].reason.as_deref(), Some("record_failed"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_left_out_of_a_check_keeps_its_status(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let trace = TraceContext::root(Cause::definition_change(id));
        check(&pool, ws, id, &all(RecordStatus::Ok), &trace).await;

        let partial = check(&pool, ws, id, &[], &trace).await;

        assert_eq!(partial.domain.status, "verified");
        assert!(!partial.changed());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn what_a_record_keeps_of_the_observed_text_is_bounded(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let trace = TraceContext::root(Cause::definition_change(id));
        let observed: Vec<String> = (0..20).map(|_| "x".repeat(5000)).collect();

        let result = check(
            &pool,
            ws,
            id,
            &[RecordFinding {
                kind: RecordKind::Dkim,
                status: RecordStatus::Mismatch,
                observed,
            }],
            &trace,
        )
        .await;

        assert_eq!(result.records[0].observed_text.len(), MAX_OBSERVED);
        assert_eq!(result.records[0].observed_text[0].len(), MAX_OBSERVED_CHARS);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_check_of_a_domain_that_is_gone_or_foreign_records_nothing(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let id = plant_domain(&pool, one, "example.com").await.unwrap();
        let trace = TraceContext::root(Cause::definition_change(id));
        let mut connection = pool.acquire().await.unwrap();

        let foreign = record_domain_check(
            &mut connection,
            two,
            id,
            &all(RecordStatus::Ok),
            &trace,
            None,
        )
        .await
        .unwrap();

        assert!(foreign.is_none());
        let domain = crate::find_sending_domain(&pool, one, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(domain.status, "unverified");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_check_per_domain_waits_in_the_queue_at_a_time(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let trace = TraceContext::root(Cause::manual(zirv_flow_core::UserId::new()));

        let mut connection = pool.acquire().await.unwrap();
        let first = enqueue_domain_verify(&mut connection, ws, id, trace.clone())
            .await
            .unwrap();
        let second = enqueue_domain_verify(&mut connection, ws, id, trace)
            .await
            .unwrap();

        assert!(first && !second);
        let attempt: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT last_attempt_at FROM sending_domains")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(attempt.is_some(), "a queued check stamps the attempt");
        let (kind, payload): (String, serde_json::Value) =
            sqlx::query_as("SELECT kind, payload FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, SENDING_DOMAIN_VERIFY_KIND);
        assert_eq!(payload["domain_id"], id.to_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_status_check_refuses_a_verified_domain_without_a_time(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();

        let refused = sqlx::query("UPDATE sending_domains SET status = 'verified' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;

        assert!(refused.is_err());
    }
}
