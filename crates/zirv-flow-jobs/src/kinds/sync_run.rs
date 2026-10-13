//! `sync.run`: reads one stream of a source and applies it to people (architecture sections
//! 7.1, 7.2 and 14).
//!
//! The job has three shapes. A payload with only a `source_id` is a schedule firing: it queues a
//! run for every enabled, mapped stream of the source that has no active run. A payload with a
//! `stream_id` and no `run_id` is the firing of a stream's reconciliation schedule: it queues a
//! reconciliation run of that stream if it is still eligible. A payload with a `run_id` works one
//! run: it reads the stream from the checkpoint, gathers the source's batches
//! into units of at least 500 records and commits each unit in ONE transaction of its own that
//! applies the records, writes the rejects and the decisions and advances the checkpoint, fenced
//! on the job's lease and on the run's state. The job's own transaction only holds the lease.
//! A crash or a lost lease resumes from the run's checkpoint when the job is retried; cancelling
//! the run stops it at the next unit.
//!
//! A full-refresh run and a reconciliation run end with the sweep: once the whole stream is read
//! and committed, the live records of the stream that the run did not see are removed in bounded,
//! fenced units (architecture 7.2), unless that would remove more than the stream's sweep limit.
//! A reconciliation reads the stream from the beginning for its keys only and applies nothing.
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
use zirv_flow_core::mapping::{
    self, Header, JsonRecord, Mapping, TraitState, reject_sample, validate_paths,
};
use zirv_flow_store::HmacKey;
use zirv_flow_store::secrets::{self, OpenError};
use zirv_flow_store::{
    PgPool, ProfileRecord, ReconcileQueued, RunEnd, SYNC_DELETE_BY_SIGNAL, SYNC_DELETE_BY_SOURCE,
    SYNC_RUN_JOB_KIND, SecretId, SecretPurpose, StoreError, Stream, SweepOutcome, SweepUnit,
    SyncBatch, SyncBatchOutcome, SyncCounts, SyncEvent, SyncLease, SyncReject, SyncUpsert,
    commit_sync_batch, fail_run, find_source, find_stream_by_id, finish_run,
    list_trait_definitions, queue_reconcile_run, queue_runs_for_source, run_trace, start_run,
    sweep_exceeds_limit, sweep_unreached,
};

use super::import_rows::Probe;
use super::ingest_drain::whole_then_singly;
use crate::metrics::{RunOutcome, record_sync_events, record_sync_records, record_sync_run};
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
        return match id_in(payload, "stream_id") {
            Some(stream_id) => reconcile_firing(ctx, workspace, source_id, stream_id).await,
            None => fan_out(ctx, workspace, source_id).await,
        };
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

/// A stream's reconciliation schedule fired: queues a reconciliation run of the stream. It
/// re-checks that the source and the stream are still enabled and mapped and that the stream is
/// incremental, since they may have changed since the slot was enqueued, and skips the firing
/// when the stream has an active run. The run and its job commit with this job, so a retried
/// firing queues one run.
async fn reconcile_firing(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream_id: Uuid,
) -> Result<(), JobError> {
    let queued = queue_reconcile_run(
        ctx.conn(),
        workspace,
        source_id,
        stream_id,
        zirv_flow_store::RunTrigger::Schedule,
        None,
        true,
    )
    .await?;
    match queued {
        ReconcileQueued::Queued(run) => {
            info!(run_id = %run.id, "reconciliation schedule fired");
        }
        ReconcileQueued::SkippedActive => {
            info!(source_id = %source_id, "reconciliation schedule skipped: the stream has an active run");
        }
        ReconcileQueued::Ineligible => {
            info!(source_id = %source_id, "reconciliation schedule fired for a stream that cannot reconcile");
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
    /// The keys to retract, each with why: the source said so, or the mapped delete field did.
    deleted: Vec<(String, &'static str)>,
    /// The keys of records that were read and still exist but are not applied: a
    /// reconciliation's.
    seen: Vec<String>,
    rejected: Vec<SyncReject>,
    /// Records read in the unit that a later occurrence of the same key replaced.
    duplicates: usize,
}

/// Keeps the last occurrence of each key, in source order: a unit may carry one key twice (a
/// soft-deleted row and a live one), and the later state is the source's current answer, so a key
/// is never both applied or seen and deleted, and never applied twice.
fn last_occurrence_wins(records: Vec<Record>) -> (Vec<Record>, usize) {
    let read = records.len();
    let mut last: HashMap<&str, usize> = HashMap::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        last.insert(record.key.as_str(), index);
    }
    let keep: Vec<bool> = records
        .iter()
        .enumerate()
        .map(|(index, record)| last[record.key.as_str()] == index)
        .collect();
    drop(last);
    let kept: Vec<Record> = records
        .into_iter()
        .zip(keep)
        .filter_map(|(record, keep)| keep.then_some(record))
        .collect();
    let dropped = read - kept.len();
    (kept, dropped)
}

impl Planner<'_> {
    /// Why `record` is deleted: the connector reported it, or the mapping's delete signal fires
    /// on its cells. The signal is checked before the mapping runs, so a soft-deleted row is
    /// never validated or rejected.
    fn deletion(&self, record: &Record, cells: &JsonRecord) -> Option<&'static str> {
        if record.deleted {
            Some(SYNC_DELETE_BY_SOURCE)
        } else if mapping::is_deleted(self.mapping, cells) {
            Some(SYNC_DELETE_BY_SIGNAL)
        } else {
            None
        }
    }

    /// Reads `records` for a reconciliation: only which are deleted and which still exist. The
    /// records are not mapped to profile records and nothing is rejected.
    fn plan_keys(&self, records: Vec<Record>) -> Planned {
        let (records, duplicates) = last_occurrence_wins(records);
        let mut planned = Planned {
            duplicates,
            ..Planned::default()
        };
        for record in records {
            let cells = JsonRecord::from_value_for_mapping(
                &record.data,
                self.mapping,
                self.declared.iter().copied(),
            );
            match self.deletion(&record, &cells) {
                Some(why) => planned.deleted.push((record.key, why)),
                None => planned.seen.push(record.key),
            }
        }
        planned
    }

    /// Maps `records`: the accepted ones become profile records, the deleted ones keys to
    /// retract, and the others rejects with a masked sample.
    fn plan(&self, records: Vec<Record>) -> Planned {
        let (records, duplicates) = last_occurrence_wins(records);
        let mut planned = Planned {
            duplicates,
            ..Planned::default()
        };
        for record in records {
            let cells = JsonRecord::from_value_for_mapping(
                &record.data,
                self.mapping,
                self.declared.iter().copied(),
            );
            if let Some(why) = self.deletion(&record, &cells) {
                planned.deleted.push((record.key, why));
                continue;
            }
            let sample = || {
                let sample =
                    reject_sample(self.mapping, &record.data, |key| self.plain.contains(key));
                serde_json::json!({"properties": sample.properties, "masked": sample.masked})
            };
            // The cells read for the delete signal are the ones the mapping applies to: the
            // same result as `map_json`, without reading the record twice.
            let mapped = mapping::apply(self.mapping, &cells, self.catalog);
            match mapped {
                Ok(mapped) => planned.upserts.push(SyncUpsert {
                    sample: sample(),
                    lists: mapped.lists,
                    event: mapped.event.map(|event| SyncEvent {
                        name: event.name,
                        properties: event.properties,
                        timestamp: event.occurred_at,
                    }),
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
                    record_key: record.key,
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
    /// The run is a reconciliation: it reads keys only and applies nothing.
    reconcile: bool,
    /// Redacts the events' properties.
    redaction_key: Option<Arc<HmacKey>>,
}

impl Pass<'_> {
    /// Maps and commits what was gathered, with the checkpoint that resumes after it.
    async fn commit(
        &mut self,
        records: Vec<Record>,
        checkpoint: &Checkpoint,
    ) -> Result<Unit, Failure> {
        let planned = if self.reconcile {
            self.planner.plan_keys(records)
        } else {
            self.planner.plan(records)
        };
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
            received_at: self.planner.observed_at,
            redaction_key: self.redaction_key.as_deref(),
            upserts: &planned.upserts,
            deleted: &planned.deleted,
            seen: &planned.seen,
            rejected: &planned.rejected,
            duplicates: planned.duplicates,
            checkpoint: &checkpoint.0,
            // A reconciliation never moves the stream's cursor.
            incremental: !self.reconcile && self.stream.sync_mode == SyncMode::Incremental.as_str(),
            singly: false,
        };
        self.probe.before_commit().await;
        match commit_batch(self.pool, &batch).await? {
            SyncBatchOutcome::Committed(progress) => {
                record_sync_records(&progress.batch);
                record_sync_events(&progress.events);
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
            SyncBatchOutcome::ListMissing => {
                // A list the mapping names is gone: no later batch can succeed either.
                end_failed(self.pool, self.ids, "list_missing").await?;
                Ok(Unit::Stop)
            }
            SyncBatchOutcome::Cancelled => {
                info!(run_id = %self.ids.run, "sync run stopped: it is no longer running");
                record_sync_run(RunOutcome::Cancelled, self.seconds());
                Ok(Unit::Stop)
            }
        }
    }

    /// The sweep phase: refuses it when it would remove more than the stream's sweep limit,
    /// then removes the unseen live records in bounded units until one comes back short.
    async fn sweep(&mut self, source: &str, stream: &Stream) -> Result<Unit, Failure> {
        if sweep_exceeds_limit(
            self.pool,
            self.ids.workspace,
            self.ids.run,
            source,
            &stream.name,
            stream.sweep_limit,
        )
        .await?
        {
            info!(run_id = %self.ids.run, "sync run refused to sweep: over the stream's sweep limit");
            end_failed(self.pool, self.ids, "sweep_limit").await?;
            return Ok(Unit::Stop);
        }
        let mut after = None;
        loop {
            self.probe.before_commit().await;
            let unit = SweepUnit {
                workspace: self.ids.workspace,
                run_id: self.ids.run,
                source,
                stream: &stream.name,
                job_id: self.job.0,
                lease_token: self.job.1,
                attempt: self.job.2,
                trace: self.planner.trace,
                after,
            };
            match sweep_unreached(self.pool, &unit).await? {
                SweepOutcome::Swept {
                    removed,
                    last,
                    more,
                } => {
                    record_sync_records(&SyncCounts {
                        deleted: removed,
                        ..SyncCounts::default()
                    });
                    self.committed += 1;
                    if self.probe.stop_after == Some(self.committed) {
                        return Err(Failure::job(JobError::new("stopped for a test")));
                    }
                    if !more {
                        return Ok(Unit::Continue);
                    }
                    after = last;
                }
                SweepOutcome::LeaseLost => return Err(Failure::job(LEASE_LOST)),
                SweepOutcome::Cancelled => {
                    info!(run_id = %self.ids.run, "sync run stopped: it is no longer running");
                    record_sync_run(RunOutcome::Cancelled, self.seconds());
                    return Ok(Unit::Stop);
                }
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

    // The mapping is checked against the stream as it is now: a descriptor that lost a field, or
    // a trait archived since, fails the run here instead of rejecting every record.
    let fields: Vec<String> = descriptor
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    let Ok(header) = Header::new(fields) else {
        return end_failed(pool, ids, "mapping_invalid").await;
    };
    if validate_paths(&mapping, &header, &catalog).is_err() {
        return end_failed(pool, ids, "mapping_invalid").await;
    }
    // An event's idempotency rests on the record key, so the key must be a real one, and its
    // properties are redacted with the worker's key.
    let redaction_key = ctx
        .ingest()
        .and_then(|settings| settings.redaction_key.clone());
    if mapping.has_event() {
        if stream.primary_key.is_empty() {
            return end_failed(pool, ids, "stream_not_keyed").await;
        }
        if redaction_key.is_none() {
            return end_failed(pool, ids, "event_key_unavailable").await;
        }
    }
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
    // the last run; a full refresh and a reconciliation start from the beginning.
    let reconcile = run.is_reconcile();
    let start = run
        .checkpoint
        .clone()
        .or_else(|| {
            (mode == SyncMode::Incremental && !reconcile)
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
        reconcile,
        redaction_key,
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
    // The read is complete and committed: a full refresh and a reconciliation now remove what
    // the source no longer lists. Every other way out of the pass returned above, so a failed,
    // cancelled or threshold-stopped run sweeps nothing.
    if (reconcile || mode == SyncMode::FullRefresh)
        && let Unit::Stop = pass.sweep(&source_text, &stream).await?
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
        CancelOutcome, ClaimedJob, DiscoveredStream, NewSource, NewTraitDefinition, RunKind,
        RunTrigger, StreamSettings, apply_profile_batch, begin, cancel_run, claim_job, commit,
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
            delete_when: None,
            key_column: None,
            event: None,
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
                    reconcile_schedule: None,
                    sweep_limit: 0.5,
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
                RunKind::Sync,
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
            // A record the source no longer lists is swept when the read is complete.
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
            // Both earlier records are gone after the run, which the default limit would refuse; this
            // test is about the trail of a sweep, not the guard.
            env.limit(1.0).await;
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
            (7, 5, 0, 1, 2)
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
        assert_eq!(count("contribution.removed", "removed"), 2);
        assert_eq!(count("sync.record", "deleted"), 2);
        let reasons: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT reason FROM decision_records WHERE outcome = 'deleted' ORDER BY reason",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            reasons,
            [Some("source".to_owned()), Some("swept".to_owned())]
        );
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
        // Seen records carry the run; the retracted one and the stale one are gone from the live
        // set.
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
                "stream_1-0",
                "stream_1-3",
                "stream_1-4",
                "stream_1-5",
                "stream_1-6"
            ]
        );
        for (key, last_seen) in &seen {
            assert_eq!(*last_seen, Some(run.id), "{key}");
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
                RunKind::Sync,
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
            RunKind::Sync,
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

    fn duplicate_planner<'a>(
        mapping: &'a Mapping,
        catalog: &'a HashMap<String, TraitState>,
        plain: &'a HashSet<String>,
        trace: &'a TraceContext,
    ) -> Planner<'a> {
        Planner {
            mapping,
            catalog,
            plain,
            declared: vec!["email", "name"],
            source: "s",
            stream: "stream_1",
            observed_at: OffsetDateTime::now_utc(),
            trace,
        }
    }

    fn row(key: &str, email: &str, deleted: bool) -> Record {
        Record {
            key: key.into(),
            data: json!({"email": email, "name": "n"}),
            deleted,
        }
    }

    /// The keys of a plan: (applied, deleted, rejected, seen).
    fn shape(planned: &Planned) -> (Vec<&str>, Vec<&str>, Vec<&str>, Vec<&str>) {
        (
            planned
                .upserts
                .iter()
                .map(|u| u.record.record_key.as_str())
                .collect(),
            planned.deleted.iter().map(|(k, _)| k.as_str()).collect(),
            planned
                .rejected
                .iter()
                .map(|r| r.record_key.as_str())
                .collect(),
            planned.seen.iter().map(String::as_str).collect(),
        )
    }

    #[test]
    fn the_last_occurrence_of_a_key_in_a_unit_wins() {
        let mapping: Mapping = serde_json::from_value(mapping()).unwrap();
        let catalog = HashMap::new();
        let plain = HashSet::new();
        let trace = TraceContext::root(Cause::api("t"));
        let planner = duplicate_planner(&mapping, &catalog, &plain, &trace);

        // Deleted, then live: the key stays live and is applied once.
        let planned = planner.plan(vec![
            row("a", "a@example.com", true),
            row("b", "b@example.com", false),
            row("a", "a@example.com", false),
        ]);
        assert_eq!(shape(&planned), (vec!["b", "a"], vec![], vec![], vec![]));
        assert_eq!(planned.duplicates, 1, "a dropped duplicate was still read");

        // Live, then deleted: the key is removed and not applied.
        let planned = planner.plan(vec![
            row("a", "a@example.com", false),
            row("a", "a@example.com", false),
            row("a", "a@example.com", true),
        ]);
        assert_eq!(shape(&planned), (vec![], vec!["a"], vec![], vec![]));

        // Deleted, then rejected: the rejected key is kept and counted as seen, not deleted.
        let planned = planner.plan(vec![
            row("a", "a@example.com", true),
            row("a", "not-an-email", false),
        ]);
        assert_eq!(shape(&planned), (vec![], vec![], vec!["a"], vec![]));

        // Rejected, then live: the key is applied and not rejected.
        let planned = planner.plan(vec![
            row("a", "not-an-email", false),
            row("a", "a@example.com", false),
        ]);
        assert_eq!(shape(&planned), (vec!["a"], vec![], vec![], vec![]));
    }

    #[test]
    fn a_reconciliation_unit_keeps_the_last_occurrence_of_a_key_too() {
        let mapping: Mapping = serde_json::from_value(mapping()).unwrap();
        let catalog = HashMap::new();
        let plain = HashSet::new();
        let trace = TraceContext::root(Cause::api("t"));
        let planner = duplicate_planner(&mapping, &catalog, &plain, &trace);

        let planned = planner.plan_keys(vec![
            row("a", "a@example.com", true),
            row("a", "a@example.com", false),
            row("b", "b@example.com", false),
            row("b", "b@example.com", true),
        ]);

        assert_eq!(shape(&planned), (vec![], vec!["b"], vec![], vec!["a"]));
        assert_eq!(planned.duplicates, 2);
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

    // ---- deletion: the sweep, the delete signal and reconciliation ----

    impl Env {
        /// Replaces the fake source's config.
        async fn config(&self, config: Value) {
            sqlx::query("UPDATE sources SET config = $1")
                .bind(config)
                .execute(&self.pool)
                .await
                .unwrap();
        }

        async fn delete_signal(&self, column: &str, rule: zirv_flow_core::mapping::DeleteRule) {
            let mut mapping: Mapping = serde_json::from_value(mapping()).unwrap();
            mapping.delete_when = Some(zirv_flow_core::mapping::DeleteWhen {
                column: column.into(),
                rule,
            });
            set_stream_mapping(
                &self.pool,
                self.ws,
                self.source,
                "stream_1",
                &serde_json::to_value(mapping).unwrap(),
            )
            .await
            .unwrap();
        }

        async fn limit(&self, limit: f64) {
            sqlx::query("UPDATE streams SET sweep_limit = $1")
                .bind(limit)
                .execute(&self.pool)
                .await
                .unwrap();
        }

        /// Queues a run of `kind` and claims its job.
        async fn queue_kind(&self, kind: RunKind) -> (zirv_flow_store::SyncRun, ClaimedJob) {
            let mut connection = self.pool.acquire().await.unwrap();
            let run = queue_run(
                &mut connection,
                self.ws,
                self.source,
                self.stream,
                kind,
                RunTrigger::Manual,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            drop(connection);
            (run, self.claim().await)
        }

        /// Runs a whole run of `kind` in one attempt and returns it as it ended.
        async fn finish(&self, kind: RunKind) -> zirv_flow_store::SyncRun {
            let (run, job) = self.queue_kind(kind).await;
            attempt(self, &job, &Probe::default()).await.unwrap();
            self.run(run.id).await
        }

        async fn count_dyn(&self, sql: String) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn live_keys(&self) -> Vec<String> {
            sqlx::query_scalar(
                "SELECT record_key FROM source_records WHERE deleted_at IS NULL ORDER BY record_key",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// An event of the record's person, linked to the record when `linked`.
        async fn event_of(&self, key: &str, linked: bool) {
            let (record, person): (Uuid, Uuid) =
                sqlx::query_as("SELECT id, person_id FROM source_records WHERE record_key = $1")
                    .bind(key)
                    .fetch_one(&self.pool)
                    .await
                    .unwrap();
            sqlx::query(
                "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                    source, trace_id, validation_state, source_record_id) \
                 VALUES ($1, $2, $3, 'seen', now(), now(), 'web', $4, 'valid', $5)",
            )
            .bind(Uuid::now_v7())
            .bind(self.ws.as_uuid())
            .bind(person)
            .bind(Uuid::now_v7())
            .bind(linked.then_some(record))
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn deletions(&self, run: &zirv_flow_store::SyncRun) -> Vec<String> {
            sqlx::query_scalar(
                "SELECT reason FROM decision_records \
                 WHERE outcome = 'deleted' AND cause_id = $1 ORDER BY reason, id",
            )
            .bind(run.id.to_string())
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    fn keys(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|n| format!("stream_1-{n}")).collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_full_refresh_sweeps_the_records_the_source_dropped_with_their_events(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        let first = env.finish(RunKind::Sync).await;
        assert_eq!(first.state, "succeeded");
        // Two records will be dropped, each with a linked event; a first-party event of a
        // dropped record's person and a linked event of a kept record stay.
        env.event_of("stream_1-3", true).await;
        env.event_of("stream_1-4", true).await;
        env.event_of("stream_1-4", false).await;
        env.event_of("stream_1-0", true).await;
        env.config(json!({"records": 3})).await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.state, "succeeded");
        assert_eq!(
            (
                second.records_read,
                second.records_applied,
                second.records_unchanged,
                second.records_deleted
            ),
            (3, 0, 3, 2)
        );
        assert_eq!(env.live_keys().await, keys(0..3));
        let tombstoned: Vec<String> = sqlx::query_scalar(
            "SELECT record_key FROM source_records WHERE deleted_at IS NOT NULL ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tombstoned, keys(3..5));
        // The kept records were seen by the second run, so the sweep left them alone.
        let seen: i64 = env
            .count_dyn(format!(
                "SELECT count(*) FROM source_records WHERE deleted_at IS NULL AND last_seen_run = '{}'",
                second.id
            ))
            .await;
        assert_eq!(seen, 3);
        assert_eq!(env.count("SELECT count(*) FROM events").await, 2);
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE source_record_id IS NULL")
                .await,
            1
        );
        // Both removals are decisions of the run's trace and cause, next to the store's own.
        let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, reason FROM decision_records \
             WHERE outcome = 'deleted' AND cause_id = $1",
        )
        .bind(second.id.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| { row.0 == second.trace_id && row.1 == "sync_run" && row.3 == "swept" })
        );
        assert_eq!(
            env.count(
                "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed'"
            )
            .await,
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_that_fails_on_the_source_sweeps_nothing(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        // The second read fails after its first batch, before anything is committed.
        env.config(json!({"records": 5, "batch": 2, "fail_after_batches": 1}))
            .await;
        let (run, job) = env.queue_kind(RunKind::Sync).await;
        let last = ClaimedJob {
            attempts: job.max_attempts,
            ..job
        };

        let result = attempt(&env, &last, &Probe::default()).await;

        assert!(result.is_err());
        let failed = env.run(run.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("source_error"));
        assert_eq!(env.live_keys().await, keys(0..5));
        assert_eq!(failed.records_deleted, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_cancelled_mid_stream_sweeps_nothing(pool: PgPool) {
        let env = setup(
            &pool,
            json!({"records": 1000, "batch": 100}),
            "full_refresh",
        )
        .await;
        env.finish(RunKind::Sync).await;
        // The second run is cancelled after its first unit: the 500 records it did not reach are
        // unseen, and stay live because the run never sweeps.
        let (run, job) = env.queue_kind(RunKind::Sync).await;
        let cancelling = hook({
            let pool = pool.clone();
            let (ws, source, run_id) = (env.ws, env.source, run.id);
            move |call| {
                let pool = pool.clone();
                Box::pin(async move {
                    if call == 2 {
                        let mut connection = pool.acquire().await.unwrap();
                        cancel_run(&mut connection, ws, source, run_id)
                            .await
                            .unwrap();
                    }
                })
            }
        });

        attempt(&env, &job, &cancelling).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "cancelled");
        assert_eq!(done.records_deleted, 0);
        assert_eq!(env.live_keys().await.len(), 1000);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_stopped_by_the_reject_threshold_sweeps_nothing(pool: PgPool) {
        let env = setup(&pool, json!({"records": 600, "batch": 100}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        let invalid: Vec<usize> = (0..60).collect();
        env.config(json!({"records": 600, "batch": 100, "invalid": ids(&invalid)}))
            .await;

        let failed = env.finish(RunKind::Sync).await;

        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("reject_threshold"));
        // The run stopped after the first unit, so 100 records were never reached; all 600 are
        // live, rejected ones included.
        assert_eq!(env.live_keys().await.len(), 600);
        assert_eq!(failed.records_deleted, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_the_mapping_rejects_keeps_its_earlier_contribution(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        // Records 1 and 2 now come with a bad email: the mapping rejects them, but they still
        // exist in the source.
        env.config(json!({"records": 5, "invalid": "1,2"})).await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.state, "succeeded");
        assert_eq!(second.records_rejected, 2);
        assert_eq!(second.records_deleted, 0);
        assert_eq!(env.live_keys().await, keys(0..5));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_incremental_run_never_sweeps(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "incremental").await;
        env.finish(RunKind::Sync).await;
        // The source now lists fewer records and the cursor sits past them: an incremental
        // stream cannot tell dropped from unchanged.
        env.config(json!({"records": 3})).await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.state, "succeeded");
        assert_eq!((second.records_read, second.records_deleted), (0, 0));
        assert_eq!(env.live_keys().await, keys(0..5));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_truthy_delete_signal_removes_a_flagged_record_even_if_it_is_malformed(
        pool: PgPool,
    ) {
        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        env.delete_signal("deleted_flag", zirv_flow_core::mapping::DeleteRule::Truthy)
            .await;
        // Records 1 and 3 are soft-deleted, and 1 also has a bad email: it is removed, not
        // rejected, because the signal is read before the mapping runs.
        env.config(json!({"records": 5, "soft_deleted": "1,3", "invalid": "1"}))
            .await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.state, "succeeded");
        assert_eq!(
            (
                second.records_read,
                second.records_unchanged,
                second.records_rejected,
                second.records_deleted
            ),
            (5, 3, 0, 2)
        );
        let mut expected = keys(0..5);
        expected.retain(|key| key != "stream_1-1" && key != "stream_1-3");
        assert_eq!(env.live_keys().await, expected);
        assert_eq!(env.deletions(&second).await, ["signal", "signal"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_present_delete_signal_removes_a_record_with_a_deleted_at_value(pool: PgPool) {
        let env = setup(&pool, json!({"records": 4}), "incremental").await;
        env.finish(RunKind::Sync).await;
        env.delete_signal("deleted_at", zirv_flow_core::mapping::DeleteRule::Present)
            .await;
        // An incremental stream re-reads from its cursor, which the fake ignores here: reset it
        // so the run reads everything, and soft-delete record 2.
        sqlx::query("UPDATE streams SET checkpoint = NULL")
            .execute(&pool)
            .await
            .unwrap();
        env.config(json!({"records": 4, "soft_deleted": "2"})).await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.records_deleted, 1);
        let mut expected = keys(0..4);
        expected.retain(|key| key != "stream_1-2");
        assert_eq!(env.live_keys().await, expected);
        assert_eq!(env.deletions(&second).await, ["signal"]);
        // A soft-deleted record that was never synced is simply not applied.
        env.config(json!({"records": 4, "soft_deleted": "2"})).await;
        sqlx::query("UPDATE streams SET checkpoint = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let third = env.finish(RunKind::Sync).await;
        assert_eq!((third.records_deleted, third.records_unchanged), (0, 4));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_connector_delete_flag_still_removes_with_the_reason_source(pool: PgPool) {
        let env = setup(&pool, json!({"records": 3}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        env.config(json!({"records": 3, "deleted": "0"})).await;

        let second = env.finish(RunKind::Sync).await;

        assert_eq!(second.records_deleted, 1);
        assert_eq!(env.deletions(&second).await, ["source"]);
        assert_eq!(env.live_keys().await, keys(1..3));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconciliation_removes_the_missing_record_applies_nothing_and_keeps_the_cursor(
        pool: PgPool,
    ) {
        let env = setup(&pool, json!({"records": 5}), "incremental").await;
        env.finish(RunKind::Sync).await;
        let cursor: Option<Value> = sqlx::query_scalar("SELECT checkpoint FROM streams")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cursor, Some(json!({"offset": 5})));
        // The source lost two records: a reconciliation lists keys only and applies nothing.
        env.config(json!({"records": 3, "batch": 2})).await;

        let reconcile = env.finish(RunKind::Reconcile).await;

        assert_eq!(reconcile.state, "succeeded");
        assert_eq!(reconcile.run_kind, "reconcile");
        // It read from the beginning although the stream's cursor is at the end.
        assert_eq!(reconcile.records_read, 3);
        assert_eq!(
            (reconcile.records_applied, reconcile.records_rejected),
            (0, 0)
        );
        assert_eq!(reconcile.records_deleted, 2);
        assert_eq!(env.live_keys().await, keys(0..3));
        let seen = env
            .count_dyn(format!(
                "SELECT count(*) FROM source_records WHERE deleted_at IS NULL AND last_seen_run = '{}'",
                reconcile.id
            ))
            .await;
        assert_eq!(seen, 3, "the keys it listed are marked seen");
        let after: Option<Value> = sqlx::query_scalar("SELECT checkpoint FROM streams")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, cursor, "a reconciliation never moves the cursor");
        assert_eq!(env.deletions(&reconcile).await, ["swept", "swept"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconciliation_honours_the_delete_signal_and_the_connector_flag(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "incremental").await;
        env.finish(RunKind::Sync).await;
        env.delete_signal("deleted_flag", zirv_flow_core::mapping::DeleteRule::Truthy)
            .await;
        env.config(json!({"records": 5, "soft_deleted": "1", "deleted": "2"}))
            .await;

        let reconcile = env.finish(RunKind::Reconcile).await;

        assert_eq!(reconcile.records_deleted, 2);
        assert_eq!(env.deletions(&reconcile).await, ["signal", "source"]);
        let mut expected = keys(0..5);
        expected.retain(|key| key != "stream_1-1" && key != "stream_1-2");
        assert_eq!(env.live_keys().await, expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_crash_mid_sweep_resumes_and_removes_each_record_once(pool: PgPool) {
        let env = setup(
            &pool,
            json!({"records": 450, "batch": 1000}),
            "full_refresh",
        )
        .await;
        env.finish(RunKind::Sync).await;
        // 350 of 450 are dropped, which only a raised limit allows.
        env.limit(1.0).await;
        env.config(json!({"records": 100, "batch": 1000})).await;
        let (run, job) = env.queue_kind(RunKind::Sync).await;

        // The attempt dies after the read unit and the first sweep unit of 200.
        let first = attempt(
            &env,
            &job,
            &Probe {
                stop_after: Some(2),
                before_commit: None,
            },
        )
        .await;
        assert!(first.is_err());
        let mid = env.run(run.id).await;
        assert_eq!((mid.state.as_str(), mid.records_deleted), ("running", 200));

        attempt(&env, &job, &Probe::default()).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "succeeded");
        assert_eq!(done.records_deleted, 350);
        assert_eq!(env.live_keys().await.len(), 100);
        assert_eq!(env.deletions(&done).await.len(), 350);
        assert_eq!(
            env.count(
                "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed'"
            )
            .await,
            350
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancel_arriving_before_a_sweep_unit_removes_nothing(pool: PgPool) {
        let env = setup(&pool, json!({"records": 10}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        env.config(json!({"records": 6})).await;
        let (run, job) = env.queue_kind(RunKind::Sync).await;
        let cancelling = hook({
            let pool = pool.clone();
            let (ws, source, run_id) = (env.ws, env.source, run.id);
            move |call| {
                let pool = pool.clone();
                Box::pin(async move {
                    // Call 1 is the read unit; call 2 is just before the first sweep unit.
                    if call == 2 {
                        let mut connection = pool.acquire().await.unwrap();
                        cancel_run(&mut connection, ws, source, run_id)
                            .await
                            .unwrap();
                    }
                })
            }
        });

        attempt(&env, &job, &cancelling).await.unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "cancelled");
        assert_eq!(done.records_deleted, 0);
        assert_eq!(env.live_keys().await.len(), 10);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_over_the_streams_limit_removes_nothing_and_fails_the_run(pool: PgPool) {
        let env = setup(&pool, json!({"records": 4}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        // Three of four records are gone: 75% is over the default 50%.
        env.config(json!({"records": 1})).await;

        let refused = env.finish(RunKind::Sync).await;

        assert_eq!(refused.state, "failed");
        assert_eq!(refused.error_code.as_deref(), Some("sweep_limit"));
        assert_eq!(refused.records_deleted, 0);
        assert!(refused.finished_at.is_some());
        assert_eq!(env.live_keys().await, keys(0..4));

        // An admin who raises the limit to 1 authorizes it, and the next run removes them.
        env.limit(1.0).await;
        let allowed = env.finish(RunKind::Sync).await;
        assert_eq!(allowed.state, "succeeded");
        assert_eq!(allowed.records_deleted, 3);
        assert_eq!(env.live_keys().await, keys(0..1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_changed_primary_key_is_refused_by_the_limit_and_removes_nothing(pool: PgPool) {
        let env = setup(&pool, json!({"records": 4}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        // The source now names the same four records by new keys: the ledger's old keys stay
        // behind as four unseen rows next to the four the run inserts and sees.
        sqlx::query("UPDATE source_records SET record_key = 'old-' || record_key")
            .execute(&pool)
            .await
            .unwrap();

        let refused = env.finish(RunKind::Sync).await;

        assert_eq!(refused.state, "failed");
        assert_eq!(refused.error_code.as_deref(), Some("sweep_limit"));
        assert_eq!(refused.records_deleted, 0);
        assert_eq!(env.live_keys().await.len(), 8);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_read_of_nothing_over_live_records_is_refused_and_a_drop_at_the_limit_sweeps(
        pool: PgPool,
    ) {
        let env = setup(&pool, json!({"records": 4}), "full_refresh").await;
        env.finish(RunKind::Sync).await;
        env.config(json!({"records": 0})).await;

        let empty = env.finish(RunKind::Sync).await;

        assert_eq!(empty.state, "failed");
        assert_eq!(empty.error_code.as_deref(), Some("sweep_limit"));
        assert_eq!(env.live_keys().await, keys(0..4));

        // Exactly at the limit (2 of 4 at 50%) sweeps.
        env.config(json!({"records": 2})).await;
        let at_limit = env.finish(RunKind::Sync).await;
        assert_eq!(at_limit.state, "succeeded");
        assert_eq!(at_limit.records_deleted, 2);
        assert_eq!(env.live_keys().await, keys(0..2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconciliation_over_the_limit_removes_nothing_either(pool: PgPool) {
        let env = setup(&pool, json!({"records": 4}), "incremental").await;
        env.finish(RunKind::Sync).await;
        env.config(json!({"records": 1})).await;

        let refused = env.finish(RunKind::Reconcile).await;

        assert_eq!(refused.error_code.as_deref(), Some("sweep_limit"));
        assert_eq!(env.live_keys().await, keys(0..4));
    }

    /// The payload the scheduler enqueues for a stream's reconciliation schedule.
    async fn fire_reconcile(env: &Env, slot: i64) -> ClaimedJob {
        let mut job = zirv_flow_store::NewJob::new(
            SYNC_RUN_JOB_KIND,
            TraceContext::root(Cause::schedule(Uuid::now_v7())),
        );
        job.workspace_id = Some(env.ws);
        job.payload = json!({"source_id": env.source, "stream_id": env.stream, "reconcile": true});
        job.unique_key = Some(format!("reconcile:test:{slot}"));
        zirv_flow_store::enqueue(&env.pool, &job).await.unwrap();
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconcile_schedule_firing_queues_one_run_and_skips_an_active_one(pool: PgPool) {
        let env = setup(&pool, json!({"records": 5}), "incremental").await;
        let fired = fire_reconcile(&env, 1).await;

        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();

        let runs: Vec<(String, String, String)> =
            sqlx::query_as("SELECT run_kind, trigger, state FROM sync_runs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            runs,
            [(
                "reconcile".to_owned(),
                "schedule".to_owned(),
                "queued".to_owned()
            )]
        );
        // The queued run has its own job under the run's trace; the next firing skips it.
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'sync.run' AND payload ? 'run_id'")
                .await,
            1
        );
        let again = fire_reconcile(&env, 2).await;
        attempt_as(&env, &again, &Probe::default(), None, true)
            .await
            .unwrap();
        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconcile_schedule_firing_queues_nothing_for_what_cannot_reconcile(pool: PgPool) {
        // A full-refresh stream, a disabled stream and a disabled source are all left alone.
        let env = setup(&pool, json!({"records": 5}), "full_refresh").await;
        let fired = fire_reconcile(&env, 1).await;
        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();
        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 0);

        sqlx::query("UPDATE streams SET sync_mode = 'incremental', cursor_field = 'updated_at'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE streams SET enabled = false, enabled_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let fired = fire_reconcile(&env, 2).await;
        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();
        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 0);

        sqlx::query("UPDATE streams SET enabled = true")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE sources SET enabled = false")
            .execute(&pool)
            .await
            .unwrap();
        let fired = fire_reconcile(&env, 3).await;
        attempt_as(&env, &fired, &Probe::default(), None, true)
            .await
            .unwrap();
        assert_eq!(env.count("SELECT count(*) FROM sync_runs").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_fires_a_streams_reconcile_schedule_and_the_worker_reconciles(pool: PgPool) {
        use crate::{Scheduler, SchedulerConfig, Worker, WorkerConfig};

        let env = setup(&pool, json!({"records": 5}), "incremental").await;
        env.finish(RunKind::Sync).await;
        env.config(json!({"records": 3})).await;
        let mut tx = begin(&pool).await.unwrap();
        zirv_flow_store::sync_reconcile_schedule(
            &mut tx,
            env.ws,
            env.source,
            env.stream,
            Some("* * * * *"),
            true,
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        // The first run's job is claimed and abandoned; free the stream's slot is not needed as
        // it succeeded. Make the slot due.
        sqlx::query("UPDATE schedules SET next_run_at = now() - interval '1 minute'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM jobs")
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
            let states: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT run_kind, trigger, state FROM sync_runs WHERE trigger = 'schedule'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            if states
                == [(
                    "reconcile".to_owned(),
                    "schedule".to_owned(),
                    "succeeded".to_owned(),
                )]
            {
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

        assert_eq!(env.live_keys().await, keys(0..3));
    }

    // The file source end to end: a real `file` source against a local server.

    use std::collections::HashMap;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, Response};
    use axum::routing::get;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::mapping::map_json;
    use zirv_flow_core::{HmacPurpose, HmacRoot};

    /// A file served with byte ranges; from the `flip`th request on its ETag differs.
    struct Served {
        body: Vec<u8>,
        flip: usize,
        hits: AtomicUsize,
    }

    async fn serve_range(State(served): State<Arc<Served>>, headers: HeaderMap) -> Response<Body> {
        let n = served.hits.fetch_add(1, Ordering::SeqCst);
        let etag = if n >= served.flip { "\"v2\"" } else { "\"v1\"" };
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let whole = |body: Vec<u8>| {
            Response::builder()
                .status(200)
                .header("etag", etag)
                .body(Body::from(body))
                .unwrap()
        };
        let Some(range) = text("range") else {
            return whole(served.body.clone());
        };
        if text("if-range").is_some_and(|asked| asked != etag) {
            return whole(served.body.clone());
        }
        let spec = range.strip_prefix("bytes=").unwrap_or_default();
        let (first, last) = spec.split_once('-').unwrap_or_default();
        let first: usize = first.parse().unwrap_or(0);
        let last: usize = last.parse().unwrap_or(usize::MAX);
        let len = served.body.len();
        if first >= len {
            return Response::builder().status(416).body(Body::empty()).unwrap();
        }
        let last = last.min(len - 1);
        Response::builder()
            .status(206)
            .header("etag", etag)
            .header("content-range", format!("bytes {first}-{last}/{len}"))
            .body(Body::from(served.body[first..=last].to_vec()))
            .unwrap()
    }

    async fn serve_file(body: Vec<u8>, flip: usize) -> (String, Arc<Served>) {
        let served = Arc::new(Served {
            body,
            flip,
            hits: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new()
            .route("/f", get(serve_range))
            .with_state(Arc::clone(&served));
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://127.0.0.1:{port}/f"), served)
    }

    fn loopback() -> Arc<EgressPolicy> {
        Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap())
    }

    /// Records with nested fields: people, an order each, and a newsletter flag on even ones.
    fn nested_records(count: usize) -> Vec<Value> {
        (0..count)
            .map(|n| {
                json!({
                    "id": n,
                    "profile": {"email": format!(" User{n}@Example.com "), "name": format!("User {n}")},
                    "flags": {"news": n % 2 == 0},
                    "order": {
                        "at": "2026-03-02",
                        "total": format!("{}.50", n % 100),
                        "items": [{"sku": format!("sku-{}", n % 7)}]
                    }
                })
            })
            .collect()
    }

    fn ndjson(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>()
            .into_bytes()
    }

    fn nested_mapping(news: Uuid) -> Value {
        json!({
            "columns": [
                {"column": "profile.email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]},
                {"column": "profile.name", "target": {"type": "trait", "key": "display_name"}},
                {"column": "flags.news", "target": {"type": "list", "list_id": news}}
            ],
            "event": {
                "name": "order_placed",
                "timestamp": {"column": "order.at",
                              "transforms": [{"op": "timestamp", "format": "%Y-%m-%d"}]},
                "properties": [
                    {"property": "total", "column": "order.total",
                     "transforms": [{"op": "cast", "to": "number"}]},
                    {"property": "sku", "column": "order.items.0.sku"}
                ]
            }
        })
    }

    /// A workspace with a `file` source reading `url` as `format`, one stream `records`
    /// keyed by `id` (or by nothing), and a list `news`.
    async fn setup_file(
        pool: &PgPool,
        url: &str,
        format: &str,
        primary_key: &[&str],
    ) -> (Env, Uuid) {
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
        let news = zirv_flow_store::create_list(pool, ws, "news", "", None)
            .await
            .unwrap()
            .id;
        let config = json!({"url": url, "format": format});
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "file",
                name: "export",
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
            loopback(),
            HttpLimits::default(),
            "file",
            "test",
            Arc::default(),
        )
        .unwrap();
        let found = Registry::built_in()
            .get("file")
            .unwrap()
            .discover(&context, &config, &Credential::new())
            .await
            .unwrap();
        let descriptor = serde_json::to_value(&found[0]).unwrap();
        let key: Vec<String> = primary_key
            .iter()
            .map(|field| (*field).to_owned())
            .collect();
        let mut tx = begin(pool).await.unwrap();
        let streams = upsert_discovered_streams(
            &mut tx,
            ws,
            source.id,
            &[DiscoveredStream {
                name: "records",
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &key,
                descriptor: &descriptor,
            }],
        )
        .await
        .unwrap()
        .unwrap();
        commit(tx).await.unwrap();
        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source.id,
            "records",
            &StreamSettings {
                enabled: true,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &key,
                staleness_threshold_seconds: None,
                reconcile_schedule: None,
                sweep_limit: 0.5,
            },
        )
        .await
        .unwrap();
        set_stream_mapping(pool, ws, source.id, "records", &nested_mapping(news))
            .await
            .unwrap();
        (
            Env {
                pool: pool.clone(),
                ws,
                source: source.id,
                stream: streams[0].id,
            },
            news,
        )
    }

    fn redaction_key() -> Arc<zirv_flow_store::HmacKey> {
        Arc::new(HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction))
    }

    /// One attempt of the job, reaching loopback when `allow` and holding a redaction key when
    /// `key` is given; its transaction is dropped as a failed attempt's is.
    async fn attempt_file(
        env: &Env,
        job: &ClaimedJob,
        probe: &Probe,
        allow: bool,
        key: Option<Arc<zirv_flow_store::HmacKey>>,
    ) -> Result<(), JobError> {
        let mut tx = begin(&env.pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, job)
            .with_ingest(IngestSettings {
                pool: env.pool.clone(),
                redaction_key: key,
                batch_size: 500,
                shards: 1,
            })
            .with_outbound(
                None,
                if allow { loopback() } else { Arc::default() },
                Arc::default(),
            );
        run_sync(&mut ctx, &job.payload, probe).await
    }

    async fn run_file(env: &Env, key: &Arc<zirv_flow_store::HmacKey>) -> zirv_flow_store::SyncRun {
        let (run, job) = env.queue().await;
        attempt_file(env, &job, &Probe::default(), true, Some(Arc::clone(key)))
            .await
            .unwrap();
        env.run(run.id).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_nested_json_file_gives_people_traits_events_and_list_members_and_a_second_run_changes_nothing(
        pool: PgPool,
    ) {
        let records = nested_records(40);
        let (url, _) = serve_file(
            format!(
                "[{}]",
                records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(",\n")
            )
            .into_bytes(),
            usize::MAX,
        )
        .await;
        let (env, news) = setup_file(&pool, &url, "json", &["id"]).await;
        let key = redaction_key();

        let first = run_file(&env, &key).await;

        assert_eq!(first.state, "succeeded", "{:?}", first.error_code);
        assert_eq!(
            (
                first.records_read,
                first.records_applied,
                first.records_rejected
            ),
            (40, 40, 0)
        );
        assert_eq!(env.count("SELECT count(*) FROM people").await, 40);
        assert_eq!(env.count("SELECT count(*) FROM events").await, 40);
        assert_eq!(env.count("SELECT count(*) FROM list_members").await, 20);
        // Each event is under the run's trace, with the sync_run cause on its decision.
        let traced: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE trace_id = $1")
            .bind(first.trace_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(traced, 40);
        assert_eq!(
            env.count(
                "SELECT count(*) FROM decision_records \
                 WHERE action = 'event.accepted' AND cause_kind = 'sync_run'"
            )
            .await,
            40
        );
        assert_eq!(
            env.count("SELECT count(*) FROM list_members WHERE source = 'sync'")
                .await,
            20
        );

        // Preview equals applied: the pure function the preview calls gives what was stored.
        let catalog: HashMap<String, TraitState> = [(
            "display_name".to_owned(),
            TraitState::Active(TraitType::String),
        )]
        .into();
        let mapping: Mapping = serde_json::from_value(nested_mapping(news)).unwrap();
        let declared = ["id", "profile", "flags", "order"];
        let mut expected: Vec<(String, String, String)> = Vec::new();
        let mut expected_members = 0;
        for record in &records {
            let mapped = map_json(&mapping, record, declared, &catalog).unwrap();
            expected_members += mapped.lists.len();
            let event = mapped.event.unwrap();
            assert_eq!(event.name, "order_placed");
            expected.push((
                mapped.identifiers[0].1.clone(),
                event.properties["sku"].as_str().unwrap().to_owned(),
                format!(
                    "{}|{}",
                    event.properties["total"],
                    event.occurred_at.unwrap().date()
                ),
            ));
        }
        expected.sort();
        let mut stored: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT i.value, e.properties->>'sku', \
                    (e.properties->'total')::text || '|' || (e.occurred_at AT TIME ZONE 'UTC')::date::text \
             FROM events e JOIN identities i ON i.person_id = e.person_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        stored.sort();
        assert_eq!(stored, expected);
        assert_eq!(expected_members, 20);
        let _ = news;

        // The second run reads the same file: profiles unchanged, events duplicates.
        let second = run_file(&env, &key).await;
        assert_eq!(second.state, "succeeded");
        assert_eq!(
            (
                second.records_applied,
                second.records_unchanged,
                second.records_rejected
            ),
            (0, 40, 0)
        );
        assert_eq!(env.count("SELECT count(*) FROM events").await, 40);
        assert_eq!(env.count("SELECT count(*) FROM list_members").await, 20);
        assert_eq!(
            env.count("SELECT coalesce(sum(duplicates), 0)::bigint FROM event_dedupe")
                .await,
            40
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_crashed_file_run_resumes_at_the_checkpoint_offset_and_ends_as_an_uninterrupted_run(
        pool: PgPool,
    ) {
        let records = nested_records(1500);
        let body = ndjson(&records);
        let size = body.len();
        let (url, _) = serve_file(body, usize::MAX).await;
        let (env, _) = setup_file(&pool, &url, "ndjson", &["id"]).await;
        let key = redaction_key();
        let (run, job) = env.queue().await;

        let first = attempt_file(
            &env,
            &job,
            &Probe {
                stop_after: Some(1),
                before_commit: None,
            },
            true,
            Some(Arc::clone(&key)),
        )
        .await;
        assert!(first.is_err());
        let mid = env.run(run.id).await;
        assert_eq!(mid.state, "running");
        let offset = mid.checkpoint.as_ref().unwrap()["offset"].as_u64().unwrap();
        assert!(offset > 0 && (offset as usize) < size, "{offset} of {size}");
        assert!(mid.records_read > 0 && mid.records_read < 1500);

        attempt_file(&env, &job, &Probe::default(), true, Some(Arc::clone(&key)))
            .await
            .unwrap();

        let done = env.run(run.id).await;
        assert_eq!(done.state, "succeeded");
        assert_eq!(
            (
                done.records_read,
                done.records_applied,
                done.records_rejected
            ),
            (1500, 1500, 0),
            "no row twice, none missing"
        );
        assert_eq!(env.count("SELECT count(*) FROM people").await, 1500);
        assert_eq!(env.count("SELECT count(*) FROM events").await, 1500);
        assert_eq!(
            env.count("SELECT count(DISTINCT message_id) FROM events")
                .await,
            1500
        );
        assert_eq!(done.checkpoint.unwrap()["offset"], json!(size));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_that_changes_between_windows_fails_the_run_and_keeps_the_committed_windows(
        pool: PgPool,
    ) {
        // Over two 4 MiB windows; the server's ETag changes at its third request (discovery was the
        // first, the first window the second).
        let record = json!({
            "id": 0,
            "profile": {"email": "a@example.com", "name": "x"},
            "pad": "x".repeat(8000),
            "flags": {"news": false},
            "order": {"at": "2026-03-02", "total": "1", "items": [{"sku": "s"}]}
        });
        let records: Vec<Value> = (0..700)
            .map(|n| {
                let mut record = record.clone();
                record["id"] = json!(n);
                record["profile"]["email"] = json!(format!("u{n}@example.com"));
                record
            })
            .collect();
        let body = ndjson(&records);
        assert!(body.len() > 5 * 1024 * 1024);
        let (url, _) = serve_file(body, 2).await;
        let (env, _) = setup_file(&pool, &url, "ndjson", &["id"]).await;
        let key = redaction_key();
        let (run, job) = env.queue().await;
        let last = ClaimedJob {
            attempts: job.max_attempts,
            ..job.clone()
        };

        let result = attempt_file(&env, &last, &Probe::default(), true, Some(key)).await;

        assert!(result.is_err());
        let failed = env.run(run.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("source_error"));
        let applied = env.count("SELECT count(*) FROM source_records").await;
        assert!(applied > 0 && applied < 700, "{applied}");
        assert_eq!(failed.records_applied, applied);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_whose_address_the_egress_policy_blocks_fails_the_run_and_nothing_is_asked(
        pool: PgPool,
    ) {
        let (url, served) = serve_file(ndjson(&nested_records(3)), usize::MAX).await;
        let (env, _) = setup_file(&pool, &url, "ndjson", &["id"]).await;
        let (run, job) = env.queue().await;
        let hits_before = served.hits.load(Ordering::SeqCst);
        let last = ClaimedJob {
            attempts: job.max_attempts,
            ..job.clone()
        };

        let result =
            attempt_file(&env, &last, &Probe::default(), false, Some(redaction_key())).await;

        assert!(result.is_err());
        let failed = env.run(run.id).await;
        assert_eq!(failed.error_code.as_deref(), Some("source_error"));
        assert_eq!(served.hits.load(Ordering::SeqCst), hits_before);
        assert_eq!(env.count("SELECT count(*) FROM people").await, 0);
    }

    /// Runs the one queued run of `env` once and returns the ended run.
    async fn ended(
        env: &Env,
        key: Option<Arc<zirv_flow_store::HmacKey>>,
    ) -> zirv_flow_store::SyncRun {
        let (run, job) = env.queue().await;
        attempt_file(env, &job, &Probe::default(), true, key)
            .await
            .unwrap();
        env.run(run.id).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_needs_a_keyed_stream_a_redaction_key_and_a_mapping_the_descriptor_still_fits(
        pool: PgPool,
    ) {
        let (url, _) = serve_file(ndjson(&nested_records(5)), usize::MAX).await;
        let (env, _) = setup_file(&pool, &url, "ndjson", &["id"]).await;

        // No redaction key on the worker.
        let run = ended(&env, None).await;
        assert_eq!(run.error_code.as_deref(), Some("event_key_unavailable"));
        assert_eq!(run.records_read, 0);

        // The stream's key was cleared since the mapping was saved.
        sqlx::query("UPDATE streams SET primary_key = '{}'")
            .execute(&pool)
            .await
            .unwrap();
        let run = ended(&env, Some(redaction_key())).await;
        assert_eq!(run.error_code.as_deref(), Some("stream_not_keyed"));
        sqlx::query("UPDATE streams SET primary_key = '{id}'")
            .execute(&pool)
            .await
            .unwrap();

        // The descriptor lost a field the mapping reads.
        sqlx::query(
            "UPDATE streams SET discovered = jsonb_set(discovered, '{fields}', \
                (SELECT jsonb_agg(f) FROM jsonb_array_elements(discovered->'fields') f \
                  WHERE f->>'name' <> 'order'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let run = ended(&env, Some(redaction_key())).await;
        assert_eq!(run.error_code.as_deref(), Some("mapping_invalid"));
        assert_eq!(env.count("SELECT count(*) FROM people").await, 0);
        assert_eq!(env.count("SELECT count(*) FROM events").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_deleted_since_the_mapping_was_saved_fails_the_run_before_more_is_read(
        pool: PgPool,
    ) {
        let (url, served) = serve_file(ndjson(&nested_records(10)), usize::MAX).await;
        let (env, news) = setup_file(&pool, &url, "ndjson", &["id"]).await;
        sqlx::query("DELETE FROM lists WHERE id = $1")
            .bind(news)
            .execute(&pool)
            .await
            .unwrap();
        let _ = served;

        let run = ended(&env, Some(redaction_key())).await;

        assert_eq!(run.state, "failed");
        assert_eq!(run.error_code.as_deref(), Some("list_missing"));
        assert_eq!(env.count("SELECT count(*) FROM people").await, 0);
        assert_eq!(env.count("SELECT count(*) FROM events").await, 0);
    }
}
