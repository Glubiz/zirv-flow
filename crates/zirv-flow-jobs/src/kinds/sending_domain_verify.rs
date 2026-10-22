//! `sending_domain.verify`: reads a sending domain's DKIM, SPF and DMARC records from DNS and
//! records what it found (architecture 10.2).
//!
//! The API enqueues it when a domain is created and on demand; the scheduler leader enqueues it
//! for every domain whose last check is older than `SchedulerConfig::sending_domain_recheck`
//! (6 hours by default). A domain is `verified` only while all three records are `ok`, and falls
//! back to `unverified` the first time a check finds one that is not; the store appends a decision
//! whenever the status changes.
//!
//! The matching rules are `zirv_flow_core::sending::check_record`. A name server that does not
//! answer is not a finding: the job fails and is retried with the worker's backoff, and the
//! domain keeps its last status. A DNS outage therefore never unverifies a domain; only an answer
//! that lacks the record does.

use std::time::Duration;

use serde_json::Value;
use zirv_flow_connectors::dns::{DnsError, DnsResolver};
use zirv_flow_core::sending::{RecordKind, check_record, record_host};
use zirv_flow_store::{
    DomainRecord, RecordFinding, SENDING_DOMAIN_VERIFY_KIND, SendingDomain, find_sending_domain,
    list_domain_records, record_domain_check,
};

use super::workspace_and_id;
use crate::{JobContext, JobError, JobKind};

pub(crate) struct SendingDomainVerify;

/// The longest one lookup may take, whatever the resolver's own timeouts add up to.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(20);

async fn lookup(
    resolver: &dyn DnsResolver,
    domain: &SendingDomain,
    record: &DomainRecord,
) -> Result<Option<RecordFinding>, DnsError> {
    let Some(kind) = RecordKind::parse(&record.kind) else {
        return Ok(None);
    };
    let host = record_host(kind, &domain.selector, &domain.domain);
    let observed = tokio::time::timeout(LOOKUP_DEADLINE, resolver.lookup_txt(&host))
        .await
        .map_err(|_| DnsError::Unavailable)??;
    Ok(Some(RecordFinding {
        kind,
        status: check_record(kind, &record.expected_text, &observed),
        observed,
    }))
}

/// What DNS holds for each of the domain's records, looked up together. Any lookup that gets no
/// answer fails the whole check, so a check never records half a picture.
pub(crate) async fn look_up_records(
    resolver: &dyn DnsResolver,
    domain: &SendingDomain,
    records: &[DomainRecord],
) -> Result<Vec<RecordFinding>, DnsError> {
    let lookups = records
        .iter()
        .map(|record| lookup(resolver, domain, record));
    let mut findings = Vec::with_capacity(records.len());
    for result in futures_util::future::join_all(lookups).await {
        findings.extend(result?);
    }
    Ok(findings)
}

impl JobKind for SendingDomainVerify {
    const KIND: &'static str = SENDING_DOMAIN_VERIFY_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, domain_id) = workspace_and_id(
            &payload,
            "domain_id",
            "the payload names no workspace and domain",
        )?;
        let resolver = ctx
            .dns()
            .cloned()
            .ok_or(JobError::new("no DNS resolver is configured"))?;
        // A domain deleted since the job was queued has nothing left to check.
        let Some(domain) = find_sending_domain(ctx.conn(), workspace, domain_id).await? else {
            return Ok(());
        };
        let records = list_domain_records(ctx.conn(), workspace, domain_id).await?;
        let findings = look_up_records(resolver.as_ref(), &domain, &records)
            .await
            .map_err(|_| JobError::new("the name servers did not answer"))?;
        let trace = ctx.trace().clone();
        let job = (ctx.job_id(), ctx.attempts());
        record_domain_check(
            ctx.conn(),
            workspace,
            domain_id,
            &findings,
            &trace,
            Some(job),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_connectors::dns::FixtureResolver;
    use zirv_flow_core::{Cause, TraceContext, WorkspaceId};
    use zirv_flow_store::sending_fixtures::plant_domain;
    use zirv_flow_store::{NewJob, SendingDomain, list_trace};

    use super::*;

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    /// Queues the job for the domain, claims it and runs it once in its own transaction.
    async fn run(
        pool: &PgPool,
        resolver: &FixtureResolver,
        ws: WorkspaceId,
        domain: uuid::Uuid,
    ) -> (Result<(), JobError>, TraceContext) {
        let trace = TraceContext::root(Cause::schedule(domain));
        let mut new = NewJob::new(SendingDomainVerify::KIND, trace.clone());
        new.workspace_id = Some(ws);
        new.payload = json!({"workspace_id": ws.to_string(), "domain_id": domain.to_string()});
        zirv_flow_store::enqueue(pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[SendingDomainVerify::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let outcome = {
            let dns: Arc<dyn DnsResolver> = Arc::new(resolver.clone());
            let mut ctx = JobContext::new(&mut tx, &claimed).with_dns(Some(dns));
            SendingDomainVerify
                .run(&mut ctx, claimed.payload.clone())
                .await
        };
        match &outcome {
            Ok(()) => zirv_flow_store::commit(tx).await.unwrap(),
            Err(_) => drop(tx),
        }
        (outcome, claimed.trace)
    }

    async fn domain(pool: &PgPool, ws: WorkspaceId, id: uuid::Uuid) -> SendingDomain {
        zirv_flow_store::find_sending_domain(pool, ws, id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The text each record expects, so the fixture can publish exactly it.
    async fn expected(pool: &PgPool, ws: WorkspaceId, id: uuid::Uuid) -> [String; 3] {
        let records = list_domain_records(pool, ws, id).await.unwrap();
        [
            records[0].expected_text.clone(),
            records[1].expected_text.clone(),
            records[2].expected_text.clone(),
        ]
    }

    #[test]
    fn the_kind_name_is_sending_domain_verify() {
        assert_eq!(SendingDomainVerify::KIND, "sending_domain.verify");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_stays_unverified_until_spf_dkim_and_dmarc_all_resolve_and_drops_back(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let [dkim, spf, dmarc] = expected(&pool, ws, id).await;
        let dns = FixtureResolver::new();
        let statuses = |records: &[DomainRecord]| -> Vec<String> {
            records.iter().map(|record| record.status.clone()).collect()
        };

        // Nothing published: every record is missing.
        let (outcome, _) = run(&pool, &dns, ws, id).await;
        outcome.unwrap();
        assert_eq!(domain(&pool, ws, id).await.status, "unverified");
        assert_eq!(
            statuses(&list_domain_records(&pool, ws, id).await.unwrap()),
            ["missing", "missing", "missing"]
        );

        // DKIM and DMARC resolve, SPF is wrong: still unverified, and the wrong one says why.
        dns.set("zirv._domainkey.example.com", &[&dkim]);
        dns.set("example.com", &["v=spf1 -all", "v=spf1 mx"]);
        dns.set("_dmarc.example.com", &[&dmarc]);
        run(&pool, &dns, ws, id).await.0.unwrap();
        assert_eq!(domain(&pool, ws, id).await.status, "unverified");
        let records = list_domain_records(&pool, ws, id).await.unwrap();
        assert_eq!(statuses(&records), ["ok", "mismatch", "ok"]);
        assert_eq!(records[1].observed_text, ["v=spf1 -all", "v=spf1 mx"]);

        // A wrong DKIM key is a mismatch too.
        dns.set("example.com", &[&spf]);
        dns.set("zirv._domainkey.example.com", &["v=DKIM1; k=rsa; p=AAAA"]);
        run(&pool, &dns, ws, id).await.0.unwrap();
        assert_eq!(domain(&pool, ws, id).await.status, "unverified");

        // All three right: verified, with the time recorded.
        dns.set("zirv._domainkey.example.com", &[&dkim]);
        let (outcome, up) = run(&pool, &dns, ws, id).await;
        outcome.unwrap();
        let verified = domain(&pool, ws, id).await;
        assert_eq!(verified.status, "verified");
        assert!(verified.verified_at.is_some() && verified.last_checked_at.is_some());

        // A record disappears: back to unverified, with a decision.
        dns.remove("_dmarc.example.com");
        let (outcome, down) = run(&pool, &dns, ws, id).await;
        outcome.unwrap();
        assert_eq!(domain(&pool, ws, id).await.status, "unverified");
        assert_eq!(
            statuses(&list_domain_records(&pool, ws, id).await.unwrap()),
            ["ok", "ok", "missing"]
        );
        let (up_log, _) = list_trace(&pool, ws, up.trace_id, 10).await.unwrap();
        let (down_log, _) = list_trace(&pool, ws, down.trace_id, 10).await.unwrap();
        assert_eq!(
            (up_log.len(), up_log[0].outcome.as_str(), up_log[0].attempt),
            (1, "verified", Some(1))
        );
        assert_eq!(
            (down_log.len(), down_log[0].outcome.as_str()),
            (1, "unverified")
        );
        assert_eq!(down_log[0].action, "sending_domain.verify");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn name_servers_that_do_not_answer_fail_the_job_and_leave_the_domain_as_it_was(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let [dkim, spf, dmarc] = expected(&pool, ws, id).await;
        let dns = FixtureResolver::new();
        dns.set("zirv._domainkey.example.com", &[&dkim]);
        dns.set("example.com", &[&spf]);
        dns.set("_dmarc.example.com", &[&dmarc]);
        run(&pool, &dns, ws, id).await.0.unwrap();
        assert_eq!(domain(&pool, ws, id).await.status, "verified");
        let checked = domain(&pool, ws, id).await.last_checked_at;

        dns.fail("_dmarc.example.com");
        let (outcome, _) = run(&pool, &dns, ws, id).await;

        assert_eq!(
            outcome.unwrap_err().message(),
            "the name servers did not answer"
        );
        let after = domain(&pool, ws, id).await;
        assert_eq!(after.status, "verified");
        assert_eq!(after.last_checked_at, checked);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_deleted_domain_ends_the_job_quietly_and_no_resolver_fails_it(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        let dns = FixtureResolver::new();

        let (gone, _) = run(&pool, &dns, ws, uuid::Uuid::now_v7()).await;
        assert!(gone.is_ok());

        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let mut new = NewJob::new(
            SendingDomainVerify::KIND,
            TraceContext::root(Cause::schedule(id)),
        );
        new.payload = json!({"workspace_id": ws.to_string(), "domain_id": id.to_string()});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[SendingDomainVerify::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);
        let outcome = SendingDomainVerify
            .run(&mut ctx, claimed.payload.clone())
            .await;
        assert_eq!(
            outcome.unwrap_err().message(),
            "no DNS resolver is configured"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_without_a_domain_fails_the_job(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let dns = FixtureResolver::new();
        let trace = TraceContext::root(Cause::schedule(uuid::Uuid::now_v7()));
        let mut new = NewJob::new(SendingDomainVerify::KIND, trace);
        new.payload = json!({});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[SendingDomainVerify::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let dns: Arc<dyn DnsResolver> = Arc::new(dns);
        let mut ctx = JobContext::new(&mut tx, &claimed).with_dns(Some(dns));

        let outcome = SendingDomainVerify.run(&mut ctx, json!({})).await;

        assert!(outcome.is_err());
    }
}
