//! `sync.run`: reads one stream of a source and applies it to people (architecture sections
//! 7.1, 7.2 and 14).
//!
//! The job has two shapes. A payload with only a `source_id` is a schedule firing: it queues a
//! run for every enabled, mapped stream of the source that has no active run. A payload with a
//! `run_id` works one run: it reads the stream from the checkpoint, gathers the source's batches
//! into units of at least 500 records and commits each unit in ONE transaction of its own that
//! applies the records, writes the rejects and the decisions and advances the checkpoint, fenced
//! on the job's lease and on the run's state. The job's own transaction only holds the lease.
//! A crash or a lost lease resumes from the run's checkpoint when the job is retried; cancelling
//! the run stops it at the next unit.
//!
//! The credential is opened right before the source is read and dropped with it.
//!
//! Nothing here logs a record value, an identifier, a trait value or a credential.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use time::OffsetDateTime;
use tracing::{info, warn};
use uuid::Uuid;
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::sources::{
    Checkpoint, Credential, Record, Registry, SourceContext, StreamConfig, StreamDescriptor,
    StreamSettingsInput, SyncMode, validate_stream_settings,
};
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::{self, JsonRecord, Mapping, TraitState, reject_sample};
use zirv_flow_store::secrets::{self, OpenError};
use zirv_flow_store::{
    PgPool, ProfileRecord, RunEnd, SYNC_RUN_JOB_KIND, SecretId, SecretPurpose, StoreError, Stream,
    SyncBatch, SyncBatchOutcome, SyncLease, SyncReject, SyncUpsert, commit_sync_batch, fail_run,
    find_source, find_stream_by_id, finish_run, list_trait_definitions, queue_runs_for_source,
    run_trace, start_run,
};

use super::import_rows::Probe;
use super::ingest_drain::whole_then_singly;
use crate::metrics::{RunOutcome, record_sync_records, record_sync_run};
use crate::{JobContext, JobError, JobKind};

/// A unit of work commits once it holds at least this many records, or the stream ends. A
/// single source batch over the size is committed alone: checkpoints are batch-granular.
pub(crate) const SYNC_BATCH_RECORDS: usize = 500;

/// The longest the source may take to produce its next batch.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct SyncRunKind;

impl JobKind for SyncRunKind {
    const KIND: &'static str = SYNC_RUN_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        run_sync(ctx, &payload, &Probe::default()).await
    }
}

const CONFLICTED: JobError = JobError::new("the sync batch kept conflicting with another");
const LEASE_LOST: JobError = JobError::new("the sync attempt lost its lease");

/// Why an attempt failed, and the code its run ends with should no attempt be left.
struct Failure {
    error: JobError,
    code: &'static str,
}

impl Failure {
    const fn job(error: JobError) -> Self {
        Self {
            error,
            code: "job_failed",
        }
    }
}

impl From<JobError> for Failure {
    fn from(error: JobError) -> Self {
        Self::job(error)
    }
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Self::job(error.into())
    }
}

impl From<sqlx::Error> for Failure {
    fn from(error: sqlx::Error) -> Self {
        Self::job(error.into())
    }
}

fn id_in(payload: &Value, key: &str) -> Option<Uuid> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
}

/// Runs the job: a fan-out, or one run. When an attempt of a run fails and none is left, the
/// run is marked `failed`, so it never stays `running` without a job.
pub(super) async fn run_sync(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    probe: &Probe,
) -> Result<(), JobError> {
    let source_id =
        id_in(payload, "source_id").ok_or(JobError::new("the payload has no source_id"))?;
    let workspace = ctx
        .workspace_id()
        .ok_or(JobError::new("the sync job has no workspace"))?;
    if payload.get("run_id").is_none() {
        return fan_out(ctx, workspace, source_id).await;
    }
    let run_id = id_in(payload, "run_id").ok_or(JobError::new("the payload has a bad run_id"))?;
    let stream_id =
        id_in(payload, "stream_id").ok_or(JobError::new("the payload has no stream_id"))?;
    let last = ctx.attempts() >= ctx.max_attempts();
    let pool = ctx
        .pool()
        .ok_or(JobError::new("the worker has no pool for sync runs"))?
        .clone();
    let ids = Ids {
        workspace,
        source: source_id,
        stream: stream_id,
        run: run_id,
        lease: SyncLease {
            job_id: ctx.job_id(),
            token: ctx.lease_token(),
        },
    };
    match run_pass(ctx, &pool, ids, probe).await {
        Ok(()) => Ok(()),
        Err(failure) => {
            // A lost lease writes nothing: the run belongs to whoever holds the job now, or to the
            // reconciliation of dead jobs.
            if last
                && matches!(
                    fail_run(&pool, workspace, run_id, ids.lease, failure.code).await,
                    Ok(RunEnd::Ended)
                )
            {
                record_sync_run(RunOutcome::Failed, 0.0);
            }
            Err(failure.error)
        }
    }
}

/// A schedule fired for a source: queues a run per eligible stream. It re-checks eligibility,
/// since the source or its streams may have changed since the slot was enqueued. The runs and
/// their jobs commit with this job, so a retried fan-out queues each run once.
async fn fan_out(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    source_id: Uuid,
) -> Result<(), JobError> {
    let fan = queue_runs_for_source(
        ctx.conn(),
        workspace,
        source_id,
        zirv_flow_store::RunTrigger::Schedule,
        None,
        true,
    )
    .await?;
    match fan {
        Some(fan) => info!(
            source_id = %source_id,
            queued = fan.created.len(),
            skipped_active = fan.skipped_active,
            "sync schedule fired"
        ),
        None => {
            info!(source_id = %source_id, "sync schedule fired for a source that is gone or disabled")
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Ids {
    workspace: WorkspaceId,
    source: Uuid,
    stream: Uuid,
    run: Uuid,
    lease: SyncLease,
}

/// Ends the run with `code`, unless it ended already.
async fn end_failed(pool: &PgPool, ids: Ids, code: &'static str) -> Result<(), Failure> {
    match fail_run(pool, ids.workspace, ids.run, ids.lease, code).await? {
        RunEnd::Ended => record_sync_run(RunOutcome::Failed, 0.0),
        RunEnd::Over => {}
        RunEnd::LeaseLost => return Err(Failure::job(LEASE_LOST)),
    }
    Ok(())
}

/// What mapping needs besides the records.
struct Planner<'a> {
    mapping: &'a Mapping,
    catalog: &'a HashMap<String, TraitState>,
    /// The trait keys the registry marks non-PII.
    plain: &'a HashSet<String>,
    declared: Vec<&'a str>,
    source: &'a str,
    stream: &'a str,
    observed_at: OffsetDateTime,
    trace: &'a zirv_flow_core::TraceContext,
}

/// The parts of one unit of work.
#[derive(Default)]
struct Planned {
    upserts: Vec<SyncUpsert>,
    deleted: Vec<String>,
    rejected: Vec<SyncReject>,
}

impl Planner<'_> {
    /// Maps `records`: the accepted ones become profile records, the deleted ones keys to
    /// retract, and the others rejects with a masked sample.
    fn plan(&self, records: Vec<Record>) -> Planned {
        let mut planned = Planned::default();
        for record in records {
            if record.deleted {
                planned.deleted.push(record.key);
                continue;
            }
            let cells = JsonRecord::from_value(&record.data, self.declared.iter().copied());
            let sample = || {
                let sample =
                    reject_sample(self.mapping, &record.data, |key| self.plain.contains(key));
                serde_json::json!({"properties": sample.properties, "masked": sample.masked})
            };
            match mapping::apply(self.mapping, &cells, self.catalog) {
                Ok(mapped) => planned.upserts.push(SyncUpsert {
                    sample: sample(),
                    record: ProfileRecord {
                        source: self.source.to_owned(),
                        stream: self.stream.to_owned(),
                        record_key: record.key,
                        identifiers: mapped.identifiers,
                        traits: mapped.traits,
                        observed_at: self.observed_at,
                        trace: self.trace.clone(),
                    },
                }),
                Err(errors) => planned.rejected.push(SyncReject {
                    reasons: errors
                        .into_iter()
                        .map(|error| (error.column, error.reason.as_str()))
                        .collect(),
                    sample: sample(),
                }),
            }
        }
        planned
    }
}

/// The trait registry as the mapping sees it, and the keys of the traits that are not PII.
async fn load_traits(
    pool: &PgPool,
    workspace: WorkspaceId,
) -> Result<(HashMap<String, TraitState>, HashSet<String>), StoreError> {
    let mut catalog = HashMap::new();
    let mut plain = HashSet::new();
    for definition in list_trait_definitions(pool, workspace).await? {
        if definition.archived {
            catalog.insert(definition.key, TraitState::Archived);
            continue;
        }
        if !definition.pii {
            plain.insert(definition.key.clone());
        }
        catalog.insert(definition.key, TraitState::Active(definition.trait_type));
    }
    Ok((catalog, plain))
}

/// Opens the credential of a source right before it is read.
async fn open_credential(
    ctx: &JobContext<'_>,
    pool: &PgPool,
    workspace: WorkspaceId,
    secret: Option<Uuid>,
) -> Result<Credential, Failure> {
    let Some(secret) = secret else {
        return Ok(Credential::new());
    };
    let keyring = ctx.keyring().ok_or(JobError::new(
        "the worker has no master key to open the credential",
    ))?;
    let bytes = secrets::open_json(
        pool,
        keyring,
        workspace,
        SecretId::from_uuid(secret),
        SecretPurpose::ConnectorCredential,
    )
    .await
    .map_err(|error| match error {
        OpenError::Store(error) => Failure::from(error),
        OpenError::NotFound | OpenError::Unopenable => {
            Failure::job(JobError::new("the source credential could not be opened"))
        }
    })?;
    Credential::from_json(&bytes)
        .map_err(|_| Failure::job(JobError::new("the source credential could not be read")))
}

/// One batch: whole when it can be, record by record when a record is poison.
async fn commit_batch(pool: &PgPool, batch: &SyncBatch<'_>) -> Result<SyncBatchOutcome, JobError> {
    whole_then_singly(
        CONFLICTED,
        "the sync batch could not be applied",
        |singly| async move { commit_sync_batch(pool, &SyncBatch { singly, ..*batch }).await },
    )
    .await
}

/// How the run came out of a unit of work.
enum Unit {
    Continue,
    /// The run is over: failed on its rejects, or cancelled.
    Stop,
}

/// What a pass needs to commit units.
struct Pass<'a> {
    pool: &'a PgPool,
    ids: Ids,
    stream: &'a Stream,
    planner: &'a Planner<'a>,
    job: (Uuid, Uuid, i32),
    started: Option<OffsetDateTime>,
    probe: &'a Probe,
    committed: usize,
}

impl Pass<'_> {
    /// Maps and commits what was gathered, with the checkpoint that resumes after it.
    async fn commit(
        &mut self,
        records: Vec<Record>,
        checkpoint: &Checkpoint,
    ) -> Result<Unit, Failure> {
        let planned = self.planner.plan(records);
        let source = self.ids.source.to_string();
        let batch = SyncBatch {
            workspace: self.ids.workspace,
            run_id: self.ids.run,
            stream_id: self.ids.stream,
            source: &source,
            stream: &self.stream.name,
            job_id: self.job.0,
            lease_token: self.job.1,
            attempt: self.job.2,
            trace: self.planner.trace,
            upserts: &planned.upserts,
            deleted: &planned.deleted,
            rejected: &planned.rejected,
            checkpoint: &checkpoint.0,
            incremental: self.stream.sync_mode == SyncMode::Incremental.as_str(),
            singly: false,
        };
        self.probe.before_commit().await;
        match commit_batch(self.pool, &batch).await? {
            SyncBatchOutcome::Committed(progress) => {
                record_sync_records(&progress.batch);
                if progress.threshold_exceeded {
                    info!(run_id = %self.ids.run, records_read = progress.run.read,
                        "sync run stopped by the reject threshold");
                    record_sync_run(RunOutcome::Failed, self.seconds());
                    return Ok(Unit::Stop);
                }
                self.committed += 1;
                if self.probe.stop_after == Some(self.committed) {
                    return Err(Failure::job(JobError::new("stopped for a test")));
                }
                Ok(Unit::Continue)
            }
            SyncBatchOutcome::LeaseLost => Err(Failure::job(LEASE_LOST)),
            SyncBatchOutcome::Cancelled => {
                info!(run_id = %self.ids.run, "sync run stopped: it is no longer running");
                record_sync_run(RunOutcome::Cancelled, self.seconds());
                Ok(Unit::Stop)
            }
        }
    }

    fn seconds(&self) -> f64 {
        self.started.map_or(0.0, |at| {
            (OffsetDateTime::now_utc() - at).as_seconds_f64().max(0.0)
        })
    }
}

async fn run_pass(
    ctx: &mut JobContext<'_>,
    pool: &PgPool,
    ids: Ids,
    probe: &Probe,
) -> Result<(), Failure> {
    // Cancelled, or finished by an earlier attempt: nothing to do.
    let Some(run) = start_run(pool, ids.workspace, ids.run).await? else {
        return Ok(());
    };
    let Some(source) = find_source(pool, ids.workspace, ids.source).await? else {
        return end_failed(pool, ids, "source_missing").await;
    };
    let Some(stream) = find_stream_by_id(pool, ids.workspace, ids.source, ids.stream).await? else {
        return end_failed(pool, ids, "stream_missing").await;
    };
    let Some(mapping_json) = &stream.mapping else {
        return end_failed(pool, ids, "stream_not_mapped").await;
    };
    let Ok(mapping) = serde_json::from_value::<Mapping>(mapping_json.clone()) else {
        return end_failed(pool, ids, "mapping_invalid").await;
    };

    // The residual of the source review: settings can outlive the descriptor they were set
    // against, so they are checked again against the one discovery last reported.
    let Ok(descriptor) = serde_json::from_value::<StreamDescriptor>(stream.discovered.clone())
    else {
        return end_failed(pool, ids, "stream_settings_invalid").await;
    };
    let mode = match validate_stream_settings(
        &descriptor,
        &StreamSettingsInput {
            sync_mode: &stream.sync_mode,
            cursor_field: stream.cursor_field.as_deref(),
            primary_key: &stream.primary_key,
            staleness_threshold_seconds: stream.staleness_threshold_seconds,
        },
    ) {
        Ok(mode) => mode,
        Err(_) => return end_failed(pool, ids, "stream_settings_invalid").await,
    };
    let Some(driver) = Registry::built_in().get(&source.kind) else {
        return end_failed(pool, ids, "source_kind_unavailable").await;
    };

    let (catalog, plain) = load_traits(pool, ids.workspace).await?;
    let trace = run_trace(&run);
    let source_text = ids.source.to_string();
    let planner = Planner {
        mapping: &mapping,
        catalog: &catalog,
        plain: &plain,
        declared: descriptor
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect(),
        source: &source_text,
        stream: &stream.name,
        // The time the run was started, so a resumed run observes what the first attempt did.
        observed_at: run.started_at.unwrap_or(run.created_at),
        trace: &trace,
    };

    // A run resumes from its own checkpoint; an incremental stream starts from the cursor of
    // the last run; a full refresh starts from the beginning.
    let start = run
        .checkpoint
        .clone()
        .or_else(|| {
            (mode == SyncMode::Incremental)
                .then(|| stream.checkpoint.clone())
                .flatten()
        })
        .map(Checkpoint);
    let config = StreamConfig {
        name: stream.name.clone(),
        sync_mode: mode,
        cursor_field: stream.cursor_field.clone(),
        primary_key: stream.primary_key.clone(),
    };
    let source_context = SourceContext::new(
        ctx.egress().clone(),
        HttpLimits::default(),
        driver.spec().kind,
        ids.source.to_string(),
        Arc::clone(ctx.rate_limits()),
    )
    .map(|context| context.with_rate_limit(driver.spec().rate_limit))
    .map_err(|_| JobError::new("the outbound client could not be built"))?;

    let mut pass = Pass {
        pool,
        ids,
        stream: &stream,
        planner: &planner,
        job: (ctx.job_id(), ctx.lease_token(), ctx.attempts()),
        started: run.started_at,
        probe,
        committed: 0,
    };

    // The credential is opened right before the read and dropped with it.
    let credential = open_credential(ctx, pool, ids.workspace, source.credential_secret_id).await?;
    let mut batches = driver.read(&source_context, &source.config, &credential, &config, start);
    let mut gathered: Vec<Record> = Vec::new();
    let mut pending: Option<Checkpoint> = None;
    loop {
        let next = match tokio::time::timeout(READ_TIMEOUT, batches.next()).await {
            Ok(next) => next,
            Err(_) => return Err(source_failure("the source did not answer in time")),
        };
        match next {
            Some(Ok(batch)) => {
                gathered.extend(batch.records);
                pending = Some(batch.checkpoint);
                if gathered.len() >= SYNC_BATCH_RECORDS {
                    let checkpoint = pending.take().unwrap_or(Checkpoint(Value::Null));
                    if let Unit::Stop = pass
                        .commit(std::mem::take(&mut gathered), &checkpoint)
                        .await?
                    {
                        return Ok(());
                    }
                }
            }
            Some(Err(error)) => {
                warn!(run_id = %ids.run, error = %error, "a sync read failed");
                return Err(source_failure("the source could not be read"));
            }
            None => break,
        }
    }
    if let Some(checkpoint) = pending
        && let Unit::Stop = pass.commit(gathered, &checkpoint).await?
    {
        return Ok(());
    }
    let seconds = pass.seconds();
    match finish_run(pool, ids.workspace, ids.run, ids.lease).await? {
        RunEnd::Ended => {
            info!(run_id = %ids.run, "sync run finished");
            record_sync_run(RunOutcome::Succeeded, seconds);
        }
        RunEnd::Over => {}
        RunEnd::LeaseLost => return Err(Failure::job(LEASE_LOST)),
    }
    Ok(())
}

fn source_failure(message: &'static str) -> Failure {
    Failure {
        error: JobError::new(message),
        code: "source_error",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::mapping::{ColumnRule, Target};
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{
        Cause, Keyring, MasterKey, Slug, TraceContext, WorkspaceName, seal, secret_aad,
    };
    use zirv_flow_store::{
        CancelOutcome, ClaimedJob, DiscoveredStream, NewSource, NewTraitDefinition, RunTrigger,
        StreamSettings, apply_profile_batch, begin, cancel_run, claim_job, commit,
        configure_stream, create_source, create_trait_definition, create_workspace, find_run,
        queue_run, set_stream_mapping, upsert_discovered_streams,
    };

    use super::*;
    use crate::context::IngestSettings;

    #[test]
    fn the_kind_name_is_sync_run() {
        assert_eq!(SyncRunKind::KIND, "sync.run");
    }

    #[test]
    fn a_unit_commits_at_five_hundred_records() {
        assert_eq!(SYNC_BATCH_RECORDS, 500);
    }

    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        source: Uuid,
        stream: Uuid,
    }

    fn mapping() -> Value {
        let rule = |column: &str, target: Target| ColumnRule {
            column: column.into(),
            target,
            transforms: Vec::new(),
        };
        serde_json::to_value(Mapping {
            key_column: None,
            columns: vec![
                rule(
                    "email",
                    Target::Identifier {
                        kind: "email".into(),
                    },
                ),
                rule(
                    "name",
                    Target::Trait {
                        key: "display_name".into(),
                    },
                ),
            ],
        })
        .unwrap()
    }

    /// A workspace with a fake source of `config`, whose one stream `stream_1` is enabled,
    /// mapped and read in `sync_mode`.
    async fn setup(pool: &PgPool, config: Value, sync_mode: &str) -> Env {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = create_workspace(
            pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "display_name".into(),
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
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "crm",
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let context = SourceContext::new(
            Arc::default(),
            HttpLimits::default(),
            "fake",
            "test",
            Arc::default(),
        )
        .unwrap();
        let found = Registry::built_in()
            .get("fake")
            .unwrap()
            .discover(&context, &config, &Credential::new())
            .await
            .unwrap();
        let descriptors: Vec<Value> = found
            .iter()
            .map(|descriptor| serde_json::to_value(descriptor).unwrap())
            .collect();
        let incremental = sync_mode == "incremental";
        let cursor = incremental.then_some("updated_at");
        let key = vec!["id".to_owned()];
        let discovered: Vec<DiscoveredStream<'_>> = found
            .iter()
            .zip(&descriptors)
            .map(|(descriptor, json)| DiscoveredStream {
                name: &descriptor.name,
                sync_mode,
                cursor_field: cursor,
                primary_key: &key,
                descriptor: json,
            })
            .collect();
        let mut tx = begin(pool).await.unwrap();
        let streams = upsert_discovered_streams(&mut tx, ws, source.id, &discovered)
            .await
            .unwrap()
            .unwrap();
        commit(tx).await.unwrap();
        for stream in &streams {
            configure_stream(
                &mut pool.acquire().await.unwrap(),
                ws,
                source.id,
                &stream.name,
                &StreamSettings {
                    enabled: true,
                    sync_mode,
                    cursor_field: cursor,
                    primary_key: &key,
                    staleness_threshold_seconds: None,
                },
            )
            .await
            .unwrap();
            set_stream_mapping(pool, ws, source.id, &stream.name, &mapping())
                .await
                .unwrap();
        }
        Env {
            pool: pool.clone(),
            ws,
            source: source.id,
            stream: streams[0].id,
        }
    }

    impl Env {
        /// Queues a run of the stream and claims the job that works it.
        async fn queue(&self) -> (zirv_flow_store::SyncRun, ClaimedJob) {
            let mut connection = self.pool.acquire().await.unwrap();
            let run = queue_run(
                &mut connection,
                self.ws,
                self.source,
                self.stream,
                RunTrigger::Manual,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            drop(connection);
            (run, self.claim().await)
        }

        async fn claim(&self) -> ClaimedJob {
            claim_job(
                &self.pool,
                "w",
                &["sync.run"],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
            .unwrap()
        }

        async fn run(&self, id: Uuid) -> zirv_flow_store::SyncRun {
            find_run(&self.pool, self.ws, self.source, id)
                .await
                .unwrap()
                .unwrap()
        }

        async fn count(&self, sql: &'static str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }

        /// Marks the job ready again after an attempt that died, and claims it anew.
        async fn reclaim(&self, job: &ClaimedJob) -> ClaimedJob {
            sqlx::query(
                "UPDATE jobs SET state = 'ready', lease_token = NULL, lease_expires_at = NULL, \
                    locked_by = NULL WHERE id = $1",
            )
            .bind(job.id)
            .execute(&self.pool)
            .await
            .unwrap();
            self.claim().await
        }
    }

    /// Runs the handler once in a transaction of its own, which is dropped afterwards as a
    /// worker's is when the attempt fails; with `keep`, it is committed.
    async fn attempt_as(
        env: &Env,
        job: &ClaimedJob,
        probe: &Probe,
        keyring: Option<Arc<Keyring>>,
        keep: bool,
    ) -> Result<(), JobError> {
        let mut tx = begin(&env.pool).await.unwrap();
        let result = {
            let mut ctx = JobContext::new(&mut tx, job)
                .with_ingest(IngestSettings {
                    pool: env.pool.clone(),
                    redaction_key: None,
                    batch_size: 500,
                    shards: 1,
                })
                .with_outbound(keyring, Arc::default(), Arc::default());
            run_sync(&mut ctx, &job.payload, probe).await
        };
        if keep && result.is_ok() {
            commit(tx).await.unwrap();
        }
        result
    }

    async fn attempt(env: &Env, job: &ClaimedJob, probe: &Probe) -> Result<(), JobError> {
        attempt_as(env, job, probe, None, false).await
    }

    fn hook<F>(f: F) -> Probe
    where
        F: Fn(usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let calls = Arc::new(AtomicUsize::new(0));
        Probe {
            stop_after: None,
            before_commit: Some(Box::new(
                move || f(calls.fetch_add(1, Ordering::SeqCst) + 1),
            )),
        }
    }

    fn ids(list: &[usize]) -> String {
        list.iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sync_shows_applied_rejected_and_deleted_records_under_one_trace(pool: PgPool) {
        let env = setup(
            &pool,
            json!({"records": 7, "batch": 3, "deleted": "1", "invalid": "2"}),
            "full_refresh",
        )
        .await;
        // Record 1 was synced earlier, so the source's deletion retracts it.
        let (run, job) = {
            let seed = TraceContext::root(Cause::api("seed"));
            apply_profile_batch(
                &pool,
                env.ws,
                &[ProfileRecord {
                    source: env.source.to_string(),
                    stream: "stream_1".into(),
                    record_key: "stream_1-1".into(),
                    identifiers: vec![("email".into(), "old@example.com".into())],
                    traits: serde_json::Map::new(),
                    observed_at: OffsetDateTime::now_utc(),
                    trace: seed,
                }],
            )
            .await
            .unwrap();
            // A record the source no longer lists stays: nothing sweeps.
            apply_profile_batch(
                &pool,
                env.ws,
                &[ProfileRecord {
                    source: env.source.to_string(),
                    stream: "stream_1".into(),
                    record_key: "stale".into(),
                    identifiers: vec![("email".into(), "stale@example.com".into())],
                    traits: serde_json::Map::new(),
                    observed_at: OffsetDateTime::now_utc(),
                    trace: TraceContext::root(Cause::api("seed")),
                }],
            )
            .await
            .unwrap();
            env.queue().await
        };

        attempt_as(&env, &job, &Probe::default(), None, false)
            .await
            .unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "succeeded");
        assert_eq!(done.attempt, 1);
        assert_eq!(
            (
                done.records_read,
                done.records_applied,
                done.records_unchanged,
                done.records_rejected,
                done.records_deleted
            ),
            (7, 5, 0, 1, 1)
        );
        assert!(done.finished_at.is_some());
        // One trace: every decision of the run carries it, the sync_run cause and no value.
        let decisions: Vec<(Uuid, String, String, String, String, Value)> = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, action, outcome, subject_ids \
             FROM decision_records WHERE cause_kind = 'sync_run'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            decisions
                .iter()
                .all(|d| d.0 == run.trace_id && d.2 == run.id.to_string())
        );
        let count = |action: &str, outcome: &str| {
            decisions
                .iter()
                .filter(|d| d.3 == action && d.4 == outcome)
                .count()
        };
        assert_eq!(count("sync.record", "applied"), 5);
        assert_eq!(count("sync.record", "rejected"), 1);
        assert_eq!(count("contribution.removed", "removed"), 1);
        let others: i64 = env
            .count("SELECT count(*) FROM decision_records WHERE cause_kind <> 'sync_run' AND cause_id <> 'seed'")
            .await;
        assert_eq!(others, 0);
        let shown = format!("{decisions:?}");
        assert!(!shown.contains("example.com") && !shown.contains("not-an-email"));
        // Applied decisions name the person and the source record, a reject its own id.
        let applied = decisions
            .iter()
            .find(|d| d.3 == "sync.record" && d.4 == "applied")
            .unwrap();
        assert!(applied.5["person"].is_string() && applied.5["source_record"].is_string());
        let reject_id: String = sqlx::query_scalar("SELECT id::text FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        let rejected = decisions
            .iter()
            .find(|d| d.3 == "sync.record" && d.4 == "rejected")
            .unwrap();
        assert_eq!(rejected.5["ingest_reject"], json!(reject_id));
        // The reject carries the run and a sample that keeps only the non-PII trait value.
        let (sync_run, sample, person): (Uuid, Value, Option<Uuid>) =
            sqlx::query_as("SELECT sync_run_id, sample, person_id FROM ingest_rejects")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sync_run, run.id);
        assert_eq!(person, None);
        assert_eq!(sample["properties"]["name"], json!("stream_1 record 2"));
        assert_eq!(sample["properties"]["email"], Value::Null);
        assert!(
            sample["masked"]
                .as_array()
                .unwrap()
                .contains(&json!("email"))
        );
        // Seen records carry the run; the retracted one is gone from the live set; the stale
        // one is untouched.
        let seen: Vec<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT record_key, last_seen_run FROM source_records WHERE deleted_at IS NULL \
             ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let live: Vec<_> = seen.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            live,
            [
                "stale",
                "stream_1-0",
                "stream_1-3",
                "stream_1-4",
                "stream_1-5",
                "stream_1-6"
            ]
        );
        for (key, last_seen) in &seen {
            assert_eq!(*last_seen == Some(run.id), key != "stale", "{key}");
        }
        // The job queue holds nothing back: the run finished in one attempt.
        assert_eq!(done.checkpoint, Some(json!({"offset": 7})));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_crashed_attempt_resumes_from_the_checkpoint_without_reapplying(pool: PgPool) {
        let env = setup(
            &pool,
            json!({"records": 1500, "batch": 100}),
            "full_refresh",
        )
        .await;
        let (run, job) = env.queue().await;

        let first = attempt(
            &env,
            &job,
            &Probe {
                stop_after: Some(1),
                before_commit: None,
            },
        )
        .await;
        assert!(first.is_err());
        let mid = env.run(run.id).await;
        assert_eq!((mid.state.as_str(), mid.records_read), ("running", 500));
        assert_eq!(mid.checkpoint, Some(json!({"offset": 500})));

        attempt(&env, &job, &Probe::default()).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "succeeded");
        assert_eq!(done.attempt, 2);
        // Committed batches were not read or applied again.
        assert_eq!(
            (
                done.records_read,
                done.records_applied,
                done.records_unchanged
            ),
            (1500, 1500, 0)
        );
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 1500);
        assert_eq!(
            env.count("SELECT count(*) FROM decision_records WHERE action = 'person.created'")
                .await,
            1500
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_that_lost_its_lease_commits_nothing_and_the_next_attempt_resumes(
        pool: PgPool,
    ) {
        let env = setup(
            &pool,
            json!({"records": 1000, "batch": 100}),
            "full_refresh",
        )
        .await;
        let (run, job) = env.queue().await;
        let stolen = Probe {
            stop_after: None,
            before_commit: Some(Box::new({
                let pool = pool.clone();
                let taken = Arc::new(AtomicUsize::new(0));
                let job_id = job.id;
                move || {
                    let (pool, taken) = (pool.clone(), Arc::clone(&taken));
                    Box::pin(async move {
                        // Between the second unit being read and being committed, the lease is
                        // taken over.
                        if taken.fetch_add(1, Ordering::SeqCst) == 1 {
                            sqlx::query(
                                "UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1",
                            )
                            .bind(job_id)
                            .execute(&pool)
                            .await
                            .unwrap();
                        }
                    })
                }
            })),
        };

        let lost = attempt(&env, &job, &stolen).await;

        assert_eq!(lost, Err(LEASE_LOST));
        let mid = env.run(run.id).await;
        assert_eq!((mid.state.as_str(), mid.records_read), ("running", 500));
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 500);

        let next = env.reclaim(&job).await;
        attempt(&env, &next, &Probe::default()).await.unwrap();
        let done = env.run(run.id).await;
        assert_eq!(done.state, "succeeded");
        assert_eq!((done.records_read, done.records_applied), (1000, 1000));
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 1000);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancel_stops_at_the_batch_boundary_and_leaves_the_checkpoint(pool: PgPool) {
        let env = setup(&pool, json!({"records": 1500, "batch": 100}), "incremental").await;
        let (run, job) = env.queue().await;
        let cancelling = hook({
            let pool = pool.clone();
            let (ws, source, run_id) = (env.ws, env.source, run.id);
            move |call| {
                let pool = pool.clone();
                Box::pin(async move {
                    if call == 2 {
                        let mut connection = pool.acquire().await.unwrap();
                        let outcome = cancel_run(&mut connection, ws, source, run_id)
                            .await
                            .unwrap();
                        assert!(matches!(outcome, CancelOutcome::Cancelled(_)));
                    }
                })
            }
        });

        attempt(&env, &job, &cancelling).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "cancelled");
        // The unit in flight when the cancel arrived was not applied.
        assert_eq!(done.records_read, 500);
        assert_eq!(done.checkpoint, Some(json!({"offset": 500})));
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 500);
        let stream: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(env.stream)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream, Some(json!({"offset": 500})));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_cancelled_before_it_starts_reads_nothing(pool: PgPool) {
        let env = setup(&pool, json!({}), "full_refresh").await;
        let (run, job) = env.queue().await;
        let mut connection = pool.acquire().await.unwrap();
        cancel_run(&mut connection, env.ws, env.source, run.id)
            .await
            .unwrap();
        drop(connection);

        attempt(&env, &job, &Probe::default()).await.unwrap();

        assert_eq!(env.run(run.id).await.state, "cancelled");
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn too_many_rejects_fail_the_run_and_keep_what_was_applied(pool: PgPool) {
        let invalid: Vec<usize> = (0..60).collect();
        let env = setup(
            &pool,
            json!({"records": 1000, "batch": 100, "invalid": ids(&invalid)}),
            "full_refresh",
        )
        .await;
        let (run, job) = env.queue().await;

        attempt(&env, &job, &Probe::default()).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "failed");
        assert_eq!(done.error_code.as_deref(), Some("reject_threshold"));
        // The first unit of 500 had 60 rejects (12%), over the 5% threshold: the rest was not read.
        assert_eq!(
            (
                done.records_read,
                done.records_applied,
                done.records_rejected
            ),
            (500, 440, 60)
        );
        assert_eq!(
            env.count("SELECT count(*) FROM source_records WHERE deleted_at IS NULL")
                .await,
            440
        );
        assert_eq!(
            env.count("SELECT count(*) FROM source_records WHERE deleted_at IS NOT NULL")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_incremental_run_starts_from_the_streams_checkpoint_and_a_full_refresh_from_the_start(
        pool: PgPool,
    ) {
        for (mode, second_read) in [("incremental", 3), ("full_refresh", 8)] {
            let env = setup_again(&pool, mode).await;
            let (first, job) = env.queue().await;
            attempt(&env, &job, &Probe::default()).await.unwrap();
            assert_eq!(env.run(first.id).await.records_read, 5);

            sqlx::query("UPDATE sources SET config = '{\"records\": 8}'::jsonb")
                .execute(&pool)
                .await
                .unwrap();
            let (second, job) = env.queue().await;
            attempt(&env, &job, &Probe::default()).await.unwrap();

            let done = env.run(second.id).await;
            assert_eq!(done.state, "succeeded", "{mode}");
            assert_eq!(done.records_read, second_read, "{mode}");
            cleanup(&pool).await;
        }
    }

    /// A fresh environment for each mode of one test, in a database that is cleaned between.
    async fn setup_again(pool: &PgPool, mode: &str) -> Env {
        let env = setup(pool, json!({"records": 5}), mode).await;
        sqlx::query("UPDATE streams SET checkpoint = NULL")
            .execute(pool)
            .await
            .unwrap();
        env
    }

    async fn cleanup(pool: &PgPool) {
        sqlx::query("DELETE FROM workspaces")
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stale_settings_fail_the_run_before_anything_is_read(pool: PgPool) {
        let env = setup(&pool, json!({}), "incremental").await;
        // Discovery ran again and the stream no longer has its cursor field.
        sqlx::query(
            "UPDATE streams SET discovered = jsonb_set(discovered, '{fields}', \
                (SELECT jsonb_agg(f) FROM jsonb_array_elements(discovered->'fields') f \
                 WHERE f->>'name' <> 'updated_at'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (run, job) = env.queue().await;

        attempt(&env, &job, &Probe::default()).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "failed");
        assert_eq!(done.error_code.as_deref(), Some("stream_settings_invalid"));
        assert_eq!(done.records_read, 0);
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_without_a_mapping_fails_the_run(pool: PgPool) {
        let env = setup(&pool, json!({}), "full_refresh").await;
        let (run, job) = env.queue().await;
        sqlx::query("UPDATE streams SET mapping = NULL")
            .execute(&pool)
            .await
            .unwrap();

        attempt(&env, &job, &Probe::default()).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.error_code.as_deref(), Some("stream_not_mapped"));
        assert_eq!(done.state, "failed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_read_error_is_retried_and_the_last_attempt_fails_the_run_with_source_error(
        pool: PgPool,
    ) {
        let env = setup(
            &pool,
            json!({"records": 600, "batch": 100, "fail_after_batches": 1}),
            "full_refresh",
        )
        .await;
        let (run, job) = env.queue().await;

        let first = attempt(&env, &job, &Probe::default()).await;

        assert!(first.is_err());
        let after_first = env.run(run.id).await;
        // Nothing was committed: the source failed before a unit was gathered.
        assert_eq!(after_first.state, "running");
        assert_eq!(after_first.records_read, 0);
        let last = ClaimedJob {
            attempts: job.max_attempts,
            ..env.reclaim(&job).await
        };

        let result = attempt(&env, &last, &Probe::default()).await;

        assert!(result.is_err());
        let failed = env.run(run.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("source_error"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_that_is_not_a_read_error_ends_the_run_as_job_failed_on_the_last_attempt(
        pool: PgPool,
    ) {
        let env = setup(&pool, json!({}), "full_refresh").await;
        let (run, job) = env.queue().await;
        // The source has a credential, but the worker has no master key to open it.
        let secret = zirv_flow_core::SecretId::new();
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let sealed = seal(
            &keys,
            br#"{"token":"ok"}"#,
            &secret_aad(env.ws, secret, SecretPurpose::ConnectorCredential),
        );
        zirv_flow_store::secrets::insert_unchecked(
            &pool,
            env.ws,
            SecretPurpose::ConnectorCredential,
            secret,
            &sealed,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE sources SET credential_secret_id = $1")
            .bind(secret.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let last = ClaimedJob {
            attempts: job.max_attempts,
            ..job.clone()
        };

        let result = attempt_as(&env, &last, &Probe::default(), None, false).await;

        assert!(result.is_err());
        let failed = env.run(run.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("job_failed"));
        // With the key, the credential opens and the run goes through.
        let (second, job) = {
            let mut connection = pool.acquire().await.unwrap();
            let run = queue_run(
                &mut connection,
                env.ws,
                env.source,
                env.stream,
                RunTrigger::Manual,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            drop(connection);
            (run, env.claim().await)
        };
        attempt_as(&env, &job, &Probe::default(), Some(Arc::new(keys)), false)
            .await
            .unwrap();
        assert_eq!(env.run(second.id).await.state, "succeeded");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_firing_queues_a_run_per_eligible_stream_and_skips_an_active_one(
        pool: PgPool,
    ) {
        let env = setup(&pool, json!({"streams": 3}), "full_refresh").await;
        // stream_2 has an active run; stream_3 is disabled; stream_1 is free.
        let streams = zirv_flow_store::list_streams(&pool, env.ws, env.source)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        queue_run(
            &mut connection,
            env.ws,
            env.source,
            streams[1].id,
            RunTrigger::Manual,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        drop(connection);
        sqlx::query(
            "UPDATE streams SET enabled = false, enabled_at = NULL WHERE name = 'stream_3'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut fire = NewJobFor::new(&env);
        let fired = fire.claim(&env).await;

        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();

        let runs: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT s.name, r.trigger, r.state FROM sync_runs r JOIN streams s ON s.id = r.stream_id \
             ORDER BY s.name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            runs,
            [
                (
                    "stream_1".to_owned(),
                    "schedule".to_owned(),
                    "queued".to_owned()
                ),
                (
                    "stream_2".to_owned(),
                    "manual".to_owned(),
                    "queued".to_owned()
                ),
            ]
        );
        // Each queued run has its own job under the run's trace; a second firing queues none.
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'sync.run' AND payload ? 'run_id'")
                .await,
            2
        );
        let again = fire.claim(&env).await;
        attempt_as(&env, &again, &Probe::default(), None, true)
            .await
            .unwrap();
        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_firing_for_a_disabled_source_queues_nothing(pool: PgPool) {
        let env = setup(&pool, json!({}), "full_refresh").await;
        sqlx::query("UPDATE sources SET enabled = false")
            .execute(&pool)
            .await
            .unwrap();
        let mut fire = NewJobFor::new(&env);
        let fired = fire.claim(&env).await;

        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();

        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 0);
    }

    /// The job the scheduler leader enqueues for a source's schedule.
    struct NewJobFor {
        slot: i64,
    }

    impl NewJobFor {
        fn new(_env: &Env) -> Self {
            Self { slot: 0 }
        }

        async fn claim(&mut self, env: &Env) -> ClaimedJob {
            self.slot += 1;
            let mut job = zirv_flow_store::NewJob::new(
                SYNC_RUN_JOB_KIND,
                TraceContext::root(Cause::schedule(Uuid::now_v7())),
            );
            job.workspace_id = Some(env.ws);
            job.payload = json!({"source_id": env.source});
            job.unique_key = Some(format!("schedule:test:{}", self.slot));
            zirv_flow_store::enqueue(&env.pool, &job).await.unwrap();
            // Runs queued earlier have jobs of their own; take the fan-out one.
            loop {
                let claimed = env.claim().await;
                if claimed.payload.get("run_id").is_none() {
                    return claimed;
                }
                sqlx::query("UPDATE jobs SET state = 'ready', lease_token = NULL, lease_expires_at = NULL, locked_by = NULL, run_at = now() + interval '1 hour' WHERE id = $1")
                    .bind(claimed.id)
                    .execute(&env.pool)
                    .await
                    .unwrap();
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_fires_a_sources_schedule_and_the_worker_runs_the_streams(pool: PgPool) {
        use crate::{Scheduler, SchedulerConfig, Worker, WorkerConfig};

        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        sqlx::query("UPDATE sources SET schedule = '* * * * *' WHERE id = $1")
            .bind(env.source)
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        zirv_flow_store::sync_schedule(&mut tx, env.ws, env.source, Some("* * * * *"), true)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        // Make the first slot due.
        sqlx::query("UPDATE schedules SET next_run_at = now() - interval '1 minute'")
            .execute(&pool)
            .await
            .unwrap();
        let mut leader = Scheduler::new(pool.clone(), SchedulerConfig::default());
        leader.tick_at(OffsetDateTime::now_utc()).await.unwrap();

        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = Worker::new(
            pool.clone(),
            crate::Registry::built_in(),
            WorkerConfig::new(2),
        );
        let handle = tokio::spawn(worker.run(stopped));
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let states: Vec<(String, String)> =
                sqlx::query_as("SELECT trigger, state FROM sync_runs")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            if states == [("schedule".to_owned(), "succeeded".to_owned())] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: {states:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.send(true).unwrap();
        handle.await.unwrap();

        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 5);
        // The worker completed both jobs: the fan-out and the run.
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'sync.run'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_fails_a_run_whose_job_died_and_frees_its_stream(pool: PgPool) {
        use crate::{Scheduler, SchedulerConfig};

        let env = setup(&pool, json!({}), "full_refresh").await;
        let (run, job) = env.queue().await;
        sqlx::query("UPDATE jobs SET state = 'dead' WHERE id = $1")
            .bind(job.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(env.run(run.id).await.state, "queued");
        let mut leader = Scheduler::new(pool.clone(), SchedulerConfig::default());

        leader.tick_at(OffsetDateTime::now_utc()).await.unwrap();

        let failed = env.run(run.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("job_failed"));
        let (next, _) = env.queue().await;
        assert_ne!(next.id, run.id, "the stream takes a new run");
    }

    #[test]
    fn a_nul_in_a_record_is_rejected_with_a_sample_that_holds_none() {
        let mapping: Mapping = serde_json::from_value(mapping()).unwrap();
        let catalog = HashMap::from([(
            "display_name".to_owned(),
            TraitState::Active(TraitType::String),
        )]);
        let plain = HashSet::from(["display_name".to_owned()]);
        let trace = TraceContext::root(Cause::api("t"));
        let planner = Planner {
            mapping: &mapping,
            catalog: &catalog,
            plain: &plain,
            declared: vec!["email", "name"],
            source: "s",
            stream: "stream_1",
            observed_at: OffsetDateTime::now_utc(),
            trace: &trace,
        };
        let record = Record {
            key: "k".into(),
            data: json!({"email": "not-an-email", "name": "Ad\0a", "x\0y": "1"}),
            deleted: false,
        };

        let planned = planner.plan(vec![record]);

        assert!(planned.upserts.is_empty());
        assert_eq!(planned.rejected.len(), 1);
        let sample = planned.rejected[0].sample.to_string();
        assert!(!sample.contains("\\u0000"), "{sample}");
        assert!(sample.contains("Ad\u{FFFD}a"));
    }
}
