//! What a running handler sees of its job.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_connectors::dns::DnsResolver;
use zirv_flow_connectors::runtime::RateLimits;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::{TraceContext, WorkspaceId};
use zirv_flow_render::RenderPool;

use zirv_flow_store::{
    ClaimedJob, Clock, EventRetention, HmacKey, Keyring, MessageRetention, NewDecision, NewJob,
    PgConnection, PgPool,
};

use crate::JobError;
use crate::delivery::rate::RateGates;

/// What the ingest drain needs beyond the handler's transaction: a pool for its own
/// transaction per batch (the job's transaction only holds the lease), the redaction key,
/// the batch size and the number of concurrent drain shards.
#[derive(Clone)]
pub(crate) struct IngestSettings {
    pub(crate) pool: PgPool,
    pub(crate) redaction_key: Option<Arc<HmacKey>>,
    pub(crate) batch_size: u32,
    pub(crate) shards: u32,
}

/// The handler's view of one claimed job and the transaction it runs in.
///
/// Everything the handler writes through [`conn`](Self::conn), and every job it enqueues,
/// commits together with the job's completion or not at all. A handler that returns an
/// error, or loses its lease, leaves no trace.
pub struct JobContext<'a> {
    connection: &'a mut PgConnection,
    job: &'a ClaimedJob,
    event_retention: EventRetention,
    message_retention: MessageRetention,
    ingest: Option<IngestSettings>,
    identifier_key: Option<Arc<HmacKey>>,
    ledger_dir: Option<Arc<PathBuf>>,
    keyring: Option<Arc<Keyring>>,
    egress: Arc<EgressPolicy>,
    rate_limits: Arc<RateLimits>,
    rate_gates: Arc<RateGates>,
    clock: Clock,
    render_pool: Option<RenderPool>,
    dns: Option<Arc<dyn DnsResolver>>,
    public_url: Option<String>,
    tracking_key: Option<Arc<HmacKey>>,
}

impl<'a> JobContext<'a> {
    pub(crate) fn new(connection: &'a mut PgConnection, job: &'a ClaimedJob) -> Self {
        Self {
            connection,
            job,
            event_retention: EventRetention::default(),
            message_retention: MessageRetention::default(),
            ingest: None,
            identifier_key: None,
            ledger_dir: None,
            keyring: None,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            rate_gates: Arc::default(),
            clock: Clock::default(),
            render_pool: None,
            dns: None,
            public_url: None,
            tracking_key: None,
        }
    }

    /// Gives the handler the public URL and the key that tracking links are built and signed with.
    pub(crate) fn with_tracking(
        mut self,
        public_url: Option<String>,
        key: Option<Arc<HmacKey>>,
    ) -> Self {
        self.public_url = public_url;
        self.tracking_key = key;
        self
    }

    /// The public URL (no trailing slash) and the tracking key; none unless the worker was given
    /// both.
    pub(crate) fn tracking(&self) -> Option<(&str, &Arc<HmacKey>)> {
        Some((
            self.public_url.as_deref()?.trim_end_matches('/'),
            self.tracking_key.as_ref()?,
        ))
    }

    /// Gives the handler the DNS resolver.
    pub(crate) fn with_dns(mut self, dns: Option<Arc<dyn DnsResolver>>) -> Self {
        self.dns = dns;
        self
    }

    /// The resolver DNS checks read through; none when the worker was given none.
    pub fn dns(&self) -> Option<&Arc<dyn DnsResolver>> {
        self.dns.as_ref()
    }

    /// Gives the handler the isolated renderer.
    pub(crate) fn with_render_pool(mut self, pool: Option<RenderPool>) -> Self {
        self.render_pool = pool;
        self
    }

    /// The isolated renderer (architecture 11.9); none when the worker was given no pool.
    pub fn render_pool(&self) -> Option<&RenderPool> {
        self.render_pool.as_ref()
    }

    /// Gives the handler the clock segment paths read.
    pub(crate) fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The clock the segment paths read: the database's, or a test's.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Gives the handler the operator's event retention settings.
    pub(crate) fn with_event_retention(mut self, settings: EventRetention) -> Self {
        self.event_retention = settings;
        self
    }

    /// The operator's event retention settings.
    pub fn event_retention(&self) -> &EventRetention {
        &self.event_retention
    }

    /// Gives the handler the operator's message retention settings.
    pub(crate) fn with_message_retention(mut self, settings: MessageRetention) -> Self {
        self.message_retention = settings;
        self
    }

    /// The operator's message retention settings.
    pub fn message_retention(&self) -> &MessageRetention {
        &self.message_retention
    }

    /// Gives the handler what the ingest drain needs.
    pub(crate) fn with_ingest(mut self, settings: IngestSettings) -> Self {
        self.ingest = Some(settings);
        self
    }

    /// Gives the handler the key that hashes identifiers.
    pub(crate) fn with_identifier_key(mut self, key: Option<Arc<HmacKey>>) -> Self {
        self.identifier_key = key;
        self
    }

    /// Gives the handler the directory the erasure ledger is copied to (`privacy.ledger_dir`).
    pub(crate) fn with_ledger_dir(mut self, dir: Option<Arc<PathBuf>>) -> Self {
        self.ledger_dir = dir;
        self
    }

    /// Gives the handler the master keys that open stored secrets and the egress policy that
    /// outbound calls follow.
    pub(crate) fn with_outbound(
        mut self,
        keyring: Option<Arc<Keyring>>,
        egress: Arc<EgressPolicy>,
        rate_limits: Arc<RateLimits>,
    ) -> Self {
        self.keyring = keyring;
        self.egress = egress;
        self.rate_limits = rate_limits;
        self
    }

    /// Gives the handler the process's rate gates, one per destination.
    pub(crate) fn with_rate_gates(mut self, gates: Arc<RateGates>) -> Self {
        self.rate_gates = gates;
        self
    }

    /// The rate gates of the destinations this process sends to.
    pub(crate) fn rate_gates(&self) -> &Arc<RateGates> {
        &self.rate_gates
    }

    /// The master keys that open stored secrets; none when the worker has no master key.
    pub(crate) fn keyring(&self) -> Option<&Arc<Keyring>> {
        self.keyring.as_ref()
    }

    /// Which non-public destinations outbound connections may reach.
    pub(crate) fn rate_limits(&self) -> &Arc<RateLimits> {
        &self.rate_limits
    }

    pub(crate) fn egress(&self) -> &Arc<EgressPolicy> {
        &self.egress
    }

    /// The token of the lease this attempt holds. Writes that must not happen after the lease
    /// is lost lock it first (`zirv_flow_store::lock_lease`).
    pub(crate) fn lease_token(&self) -> Uuid {
        self.job.lease_token
    }

    /// The key that hashes identifiers; none when the worker has no master key.
    pub(crate) fn identifier_key(&self) -> Option<Arc<HmacKey>> {
        self.identifier_key.clone()
    }

    /// Where completed erasures are copied; none when `privacy.ledger_dir` is unset.
    pub(crate) fn ledger_dir(&self) -> Option<Arc<PathBuf>> {
        self.ledger_dir.clone()
    }

    /// The ingest drain's settings; none when the worker was given no pool for them.
    pub(crate) fn ingest(&self) -> Option<&IngestSettings> {
        self.ingest.as_ref()
    }

    /// A pool of the worker's own, for handlers that commit transactions beside the job's
    /// own, such as the imports; none when the worker was given no pool for them.
    pub(crate) fn pool(&self) -> Option<&PgPool> {
        self.ingest.as_ref().map(|settings| &settings.pool)
    }

    /// The handler's transaction.
    pub fn conn(&mut self) -> &mut PgConnection {
        self.connection
    }

    pub fn job_id(&self) -> Uuid {
        self.job.id
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.job.workspace_id.map(WorkspaceId::from_uuid)
    }

    /// The attempt number, starting at 1.
    pub fn attempts(&self) -> i32 {
        self.job.attempts
    }

    /// How many attempts the job has in all before it is buried as dead.
    pub fn max_attempts(&self) -> i32 {
        self.job.max_attempts
    }

    pub fn unique_key(&self) -> Option<&str> {
        self.job.unique_key.as_deref()
    }

    /// The payload the job was enqueued with, for handlers that need to pass it on.
    pub fn payload(&self) -> &Value {
        &self.job.payload
    }

    /// The trace this job belongs to. Pass it on when enqueueing follow-up work, so the
    /// work stays on one trace.
    pub fn trace(&self) -> &TraceContext {
        &self.job.trace
    }

    /// Appends a decision to the workspace's log, in the handler's transaction, under this
    /// job's trace and cause. Only durable ids go in `subject_ids`; an instance-level job
    /// has no workspace to record in and gets an error.
    pub async fn record_decision(
        &mut self,
        action: &'static str,
        outcome: &'static str,
        reason: Option<&'static str>,
        subject_ids: Vec<(&'static str, Uuid)>,
    ) -> Result<(), JobError> {
        let Some(workspace_id) = self.workspace_id() else {
            return Err(JobError::new(
                "an instance-level job has no workspace to record a decision in",
            ));
        };
        let decision = NewDecision {
            graph: None,
            workspace_id,
            trace: self.job.trace.clone(),
            action,
            outcome,
            reason,
            subject_ids,
            job: Some((self.job.id, self.job.attempts)),
        };
        Ok(zirv_flow_store::append_decision(&mut *self.connection, &decision).await?)
    }

    /// Enqueues a follow-up job in the handler's transaction. False when its unique key is
    /// already held.
    pub async fn enqueue(&mut self, job: &NewJob) -> Result<bool, JobError> {
        Ok(zirv_flow_store::enqueue(&mut *self.connection, job).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use zirv_flow_core::Cause;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_sees_its_job_and_enqueues_in_its_own_transaction(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let trace = TraceContext::root(Cause::api("req-1"));
        let mut first = NewJob::new("a.kind", trace.clone());
        first.payload = serde_json::json!({"n": 1});
        first.unique_key = Some("k".into());
        zirv_flow_store::enqueue(&pool, &first).await.unwrap();
        let claimed =
            zirv_flow_store::claim_job(&pool, "w", &["a.kind"], Duration::from_secs(30), None)
                .await
                .unwrap()
                .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();

        let mut ctx = JobContext::new(&mut tx, &claimed);
        assert_eq!(ctx.job_id(), claimed.id);
        assert_eq!(ctx.attempts(), 1);
        assert_eq!(ctx.unique_key(), Some("k"));
        assert_eq!(ctx.workspace_id(), None);
        assert_eq!(ctx.payload(), &serde_json::json!({"n": 1}));
        assert_eq!(ctx.trace(), &trace);
        assert!(
            ctx.enqueue(&NewJob::new("b.kind", ctx.trace().clone()))
                .await
                .unwrap()
        );
        let in_tx = zirv_flow_store::queue_counts(ctx.conn()).await.unwrap();
        assert_eq!(in_tx.ready, 1);
        drop(tx);

        // Rolled back with the transaction.
        let after = zirv_flow_store::queue_counts(&pool).await.unwrap();
        assert_eq!((after.ready, after.running), (0, 1));
    }

    async fn claimed_in_transaction(
        pool: &PgPool,
        workspace: Option<WorkspaceId>,
    ) -> (ClaimedJob, zirv_flow_store::Tx) {
        let mut new = NewJob::new("a.kind", TraceContext::root(Cause::api("req-2")));
        new.workspace_id = workspace;
        zirv_flow_store::enqueue(pool, &new).await.unwrap();
        let claimed =
            zirv_flow_store::claim_job(pool, "w", &["a.kind"], Duration::from_secs(30), None)
                .await
                .unwrap()
                .unwrap();
        (claimed, zirv_flow_store::begin(pool).await.unwrap())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_recorded_decision_carries_the_jobs_workspace_trace_cause_and_attempt(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        let workspace = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let (claimed, mut tx) = claimed_in_transaction(&pool, Some(workspace)).await;
        let subject = Uuid::new_v4();

        let mut ctx = JobContext::new(&mut tx, &claimed);
        ctx.record_decision(
            "segment.evaluated",
            "matched",
            None,
            vec![("segment_id", subject)],
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let (records, _) =
            zirv_flow_store::list_trace(&pool, workspace, claimed.trace.trace_id, 10)
                .await
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action, "segment.evaluated");
        assert_eq!(records[0].outcome, "matched");
        assert_eq!(records[0].cause, claimed.trace.cause);
        assert_eq!(records[0].job_id, Some(claimed.id));
        assert_eq!(records[0].attempt, Some(1));
        assert_eq!(
            records[0].subject_ids,
            serde_json::json!({"segment_id": subject.to_string()})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_instance_level_job_cannot_record_a_decision(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (claimed, mut tx) = claimed_in_transaction(&pool, None).await;

        let mut ctx = JobContext::new(&mut tx, &claimed);
        let error = ctx
            .record_decision("anything", "done", None, Vec::new())
            .await
            .unwrap_err();

        assert_eq!(
            error.message(),
            "an instance-level job has no workspace to record a decision in"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_has_no_ingest_settings_unless_the_worker_gave_them(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (claimed, mut tx) = claimed_in_transaction(&pool, None).await;

        let ctx = JobContext::new(&mut tx, &claimed);
        assert!(ctx.ingest().is_none());
        let ctx = ctx.with_ingest(IngestSettings {
            pool: pool.clone(),
            redaction_key: None,
            batch_size: 7,
            shards: 3,
        });

        assert_eq!(ctx.ingest().unwrap().batch_size, 7);
        assert_eq!(ctx.ingest().unwrap().shards, 3);
    }
}
