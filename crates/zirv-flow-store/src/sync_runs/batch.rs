//! One batch of a sync run, in ONE transaction (architecture sections 7.1 and 7.2): the profile
//! records, the retracted contributions, the rejects, the decisions, the counters and the
//! checkpoint commit together or not at all, fenced on the job's lease and on the run's state.
//! A crash, a lost lease or a cancel therefore never leaves a half-applied batch, and a resumed
//! run neither skips nor re-applies a committed one.
//!
//! Nothing here logs a record value, an identifier or a trait value.

use hashbrown::HashMap;
use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::{HmacKey, TraceContext, WorkspaceId};

use crate::events::{
    EventBatch, EventOutcome, EventRecord, FailedItem, Refusal, apply_event_record,
    insert_record_rejects, reject_apply_failed,
};
use crate::lists::{ListSource, MAX_PEOPLE_PER_CALL, MembershipError, add_members};
use crate::people::{
    ProfileOutcome, ProfileRecord, apply_profile_records, apply_profile_records_singly,
    remove_contribution_in,
};
use crate::stream_offsets::PartitionOffset;
use crate::{
    NewDecision, PgPool, StoreError, StreamOutcome, append_decisions, begin, commit, database_now,
    lock_lease, record_stream_outcome,
};

/// A run is judged on its reject rate only once this many records have been read. Smaller than
/// the 1,000 rows of a CSV import because incremental runs are usually small.
pub const SYNC_REJECT_CHECK_MIN_RECORDS: i64 = 100;

/// Why a record was removed because the source reported it deleted.
pub const DELETE_BY_SOURCE: &str = "source";
/// Why a record was removed because the mapping's delete signal fired.
pub const DELETE_BY_SIGNAL: &str = "signal";
/// Why a record was removed because the run did not see it.
pub(crate) const DELETE_BY_SWEEP: &str = "swept";

/// A record the mapping accepted, with the masked sample to keep should the store refuse it.
#[derive(Clone, Debug)]
pub struct SyncUpsert {
    pub record: ProfileRecord,
    pub sample: Value,
    /// The lists the record's person joins.
    pub lists: Vec<Uuid>,
    /// The event the record yields, when the mapping describes one.
    pub event: Option<SyncEvent>,
    /// What tells this message from another with the same key (Kafka: `partition/offset`). The
    /// event's idempotency key hashes it in place of the record key, so a later message with
    /// the same key is not taken for a duplicate; a replayed batch reproduces the same value.
    pub occurrence: Option<String>,
}

/// The event of a record: its person is the record's, its time the run's unless it brings one.
#[derive(Clone, Debug)]
pub struct SyncEvent {
    pub name: String,
    pub properties: Map<String, Value>,
    /// When the event happened, when the mapping says.
    pub timestamp: Option<OffsetDateTime>,
}

/// A record the mapping refused: codes and a masked sample, never a value.
#[derive(Clone, Debug)]
pub struct SyncReject {
    /// The record's key. It still exists in the source, so the run counts it as seen and the
    /// sweep leaves its earlier contribution alone.
    pub record_key: String,
    /// `(field, code)` pairs; the field is empty when no one field is at fault.
    pub reasons: Vec<(String, &'static str)>,
    /// `{properties, masked}`.
    pub sample: Value,
}

/// A batch of a run: the records gathered from the source and where to resume after them.
pub struct SyncBatch<'a> {
    pub workspace: WorkspaceId,
    pub run_id: Uuid,
    pub stream_id: Uuid,
    /// The `source_records` keys of the run: the source id as text, and the stream name.
    pub source: &'a str,
    pub stream: &'a str,
    /// The job and lease the batch is fenced on, and the attempt for the decisions.
    pub job_id: Uuid,
    pub lease_token: Uuid,
    pub attempt: i32,
    pub trace: &'a TraceContext,
    /// When the run saw its records: the events' `received_at`, stable across attempts.
    pub received_at: OffsetDateTime,
    /// Redacts the events' properties. Needed when any upsert has an event.
    pub redaction_key: Option<&'a HmacKey>,
    pub upserts: &'a [SyncUpsert],
    /// The keys of the records the source or the mapping's delete signal marks as deleted, each
    /// with why: [`DELETE_BY_SOURCE`] or [`DELETE_BY_SIGNAL`].
    pub deleted: &'a [(String, &'static str)],
    /// The keys of records the run read and that still exist, but applies nothing for (a
    /// reconciliation reads keys only). They count as seen.
    pub seen: &'a [String],
    pub rejected: &'a [SyncReject],
    /// Records the unit read and dropped as an earlier occurrence of a key it carries again. They
    /// count as read, and as nothing else.
    pub duplicates: usize,
    /// The per-partition offsets a consumer resumes at after this batch; empty for a pull run.
    /// Written in this transaction, after the fence, so a batch that is not applied moves none.
    pub offsets: &'a [PartitionOffset],
    /// Where the source resumes after this batch.
    pub checkpoint: &'a Value,
    /// Also write the checkpoint to the stream, for the next run.
    pub incremental: bool,
    /// Apply each record in a savepoint of its own, so one poison record is rejected as
    /// `apply_failed` instead of stopping the run.
    pub singly: bool,
}

/// Record counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub read: i64,
    pub applied: i64,
    pub unchanged: i64,
    pub rejected: i64,
    pub deleted: i64,
}

/// What the event path did with the events of a batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventCounts {
    pub accepted: i64,
    pub duplicate: i64,
    pub rejected: i64,
}

/// Where a committed batch left the run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BatchProgress {
    /// What this batch added.
    pub batch: Counts,
    /// What became of the batch's events.
    pub events: EventCounts,
    /// The run's totals after it.
    pub run: Counts,
    /// The reject share went over the run's threshold: the batch is committed and the run is
    /// `failed` with `reject_threshold`.
    pub threshold_exceeded: bool,
}

/// What became of a batch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BatchOutcome {
    Committed(BatchProgress),
    /// The job's lease is gone: another worker owns the run now. Nothing was written.
    LeaseLost,
    /// The run is no longer running (cancelled, or ended): nothing was written.
    Cancelled,
    /// A list the mapping names does not exist: nothing was written.
    ListMissing,
}

/// Why a unit of work on a run may not proceed.
pub(crate) enum Unfenced {
    /// The job's lease is gone.
    LeaseLost,
    /// The run is no longer running.
    Cancelled,
}

/// Fences a unit of work on a run: locks the job's lease, then the run, which must be running.
/// Nothing has been written when it refuses.
pub(crate) async fn fence_run(
    tx: &mut PgConnection,
    workspace: WorkspaceId,
    run_id: Uuid,
    job_id: Uuid,
    lease_token: Uuid,
) -> Result<Option<Unfenced>, StoreError> {
    if !lock_lease(tx, job_id, lease_token).await? {
        return Ok(Some(Unfenced::LeaseLost));
    }
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM sync_runs WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    Ok((state.as_deref() != Some("running")).then_some(Unfenced::Cancelled))
}

/// Commits one batch of a run.
pub async fn commit_sync_batch(
    pool: &PgPool,
    batch: &SyncBatch<'_>,
) -> Result<BatchOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    match fence_run(
        &mut tx,
        batch.workspace,
        batch.run_id,
        batch.job_id,
        batch.lease_token,
    )
    .await?
    {
        Some(Unfenced::LeaseLost) => return Ok(BatchOutcome::LeaseLost),
        Some(Unfenced::Cancelled) => return Ok(BatchOutcome::Cancelled),
        None => {}
    }
    write_offsets(&mut tx, batch.workspace, batch.stream_id, batch.offsets).await?;

    let results = apply_upserts(&mut tx, batch).await?;
    // Every record the run read that still exists in the source is marked as seen: the applied
    // and unchanged ones, and the ones the mapping or the store refused, whose earlier
    // contribution a sweep must not take away (a broken mapping cannot cascade into deletions).
    // A record flagged deleted is not seen.
    let seen_keys: Vec<&str> = batch
        .upserts
        .iter()
        .map(|upsert| upsert.record.record_key.as_str())
        .chain(
            batch
                .rejected
                .iter()
                .map(|reject| reject.record_key.as_str()),
        )
        .chain(batch.seen.iter().map(String::as_str))
        .collect();
    let seen = mark_seen(&mut tx, batch, &seen_keys).await?;
    let events = apply_events(&mut tx, batch, &results, &seen).await?;
    link_event_rejects(&mut tx, batch, &events).await?;
    if !join_lists(&mut tx, batch, &results).await? {
        return Ok(BatchOutcome::ListMissing);
    }

    let mut counts = Counts {
        read: i64::try_from(
            batch.upserts.len()
                + batch.deleted.len()
                + batch.rejected.len()
                + batch.seen.len()
                + batch.duplicates,
        )
        .unwrap_or(i64::MAX),
        ..Counts::default()
    };
    let mut decisions = Vec::new();
    // The ledger row of each refused record, when an earlier run left one: a record refused on
    // first sight has none.
    let mut refused_records: Vec<Option<Uuid>> = batch
        .rejected
        .iter()
        .map(|reject| seen.get(reject.record_key.as_str()).copied())
        .collect();
    let mut refusals: Vec<Refusal> = batch
        .rejected
        .iter()
        .map(|reject| Refusal {
            id: Uuid::now_v7(),
            reasons: reject.reasons.clone(),
            sample: reject.sample.clone(),
        })
        .collect();
    for (index, (upsert, result)) in batch.upserts.iter().zip(results).enumerate() {
        // A record counts once, as its worst outcome: rejected, then applied, then unchanged.
        let event = events.per_upsert.get(index).copied().flatten();
        let reason: &'static str = match result {
            Some(ProfileOutcome::Applied { person_id, .. }) => {
                if event != Some(EventResult::Rejected) {
                    counts.applied += 1;
                }
                let mut subjects = vec![("person", person_id)];
                subjects.extend(
                    seen.get(upsert.record.record_key.as_str())
                        .map(|id| ("source_record", *id)),
                );
                decisions.push(decision(
                    batch,
                    &decision_actions::SYNC_RECORD_APPLIED,
                    None,
                    subjects,
                ));
                continue;
            }
            Some(ProfileOutcome::Unchanged { .. }) => {
                match event {
                    Some(EventResult::Rejected) => {}
                    Some(EventResult::Accepted) => counts.applied += 1,
                    _ => counts.unchanged += 1,
                }
                continue;
            }
            Some(ProfileOutcome::Rejected { reason }) => reason,
            None => "apply_failed",
        };
        refusals.push(Refusal {
            id: Uuid::now_v7(),
            reasons: vec![(String::new(), reason)],
            sample: upsert.sample.clone(),
        });
        refused_records.push(seen.get(upsert.record.record_key.as_str()).copied());
    }

    // The store's own `contribution.removed` decision says what happened to the person; the
    // `deleted` decision names the ledger row, as the person may go with it.
    for (key, why) in batch.deleted {
        let removed = remove_contribution_in(
            &mut tx,
            batch.workspace,
            batch.source,
            batch.stream,
            key,
            batch.trace,
        )
        .await?;
        if let Some(removed) = removed {
            counts.deleted += 1;
            decisions.push(decision(
                batch,
                &decision_actions::SYNC_RECORD_DELETED,
                Some(why),
                vec![("source_record", removed.record_id)],
            ));
        } else {
            counts.unchanged += 1;
        }
    }

    // A record whose event was refused is rejected, and its profile change stands.
    counts.rejected = i64::try_from(refusals.len())
        .unwrap_or(i64::MAX)
        .saturating_add(events.counts.rejected);
    insert_record_rejects(
        &mut tx,
        batch.workspace,
        batch.source,
        batch.stream,
        batch.trace,
        Some(batch.run_id),
        &refusals,
    )
    .await?;
    for (refusal, record) in refusals.iter().zip(refused_records) {
        let mut subjects = vec![("ingest_reject", refusal.id)];
        subjects.extend(record.map(|id| ("source_record", id)));
        let code = refusal
            .reasons
            .first()
            .map_or("rejected", |(_, code)| *code);
        decisions.push(decision(
            batch,
            &decision_actions::SYNC_RECORD_REJECTED,
            Some(code),
            subjects,
        ));
    }
    append_decisions(&mut tx, &decisions).await?;

    let (read, applied, unchanged, rejected, deleted, threshold): (i64, i64, i64, i64, i64, f64) =
        sqlx::query_as(
            "UPDATE sync_runs SET records_read = records_read + $3, \
                records_applied = records_applied + $4, \
                records_unchanged = records_unchanged + $5, \
                records_rejected = records_rejected + $6, \
                records_deleted = records_deleted + $7, \
                checkpoint = $8, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2 \
             RETURNING records_read, records_applied, records_unchanged, records_rejected, \
                       records_deleted, reject_threshold",
        )
        .bind(batch.workspace.as_uuid())
        .bind(batch.run_id)
        .bind(counts.read)
        .bind(counts.applied)
        .bind(counts.unchanged)
        .bind(counts.rejected)
        .bind(counts.deleted)
        .bind(batch.checkpoint)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::query)?;

    let exceeded = threshold_exceeded(read, rejected, threshold);
    if exceeded {
        sqlx::query(
            "UPDATE sync_runs SET state = 'failed', error_code = 'reject_threshold', \
                finished_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(batch.workspace.as_uuid())
        .bind(batch.run_id)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        let at = database_now(&mut *tx).await?;
        record_stream_outcome(
            &mut tx,
            batch.workspace,
            batch.stream_id,
            StreamOutcome::Failed,
            at,
        )
        .await?;
    } else if batch.incremental {
        // A run that failed on its rejects leaves the stream's cursor where it was, so the
        // next run reads those records again.
        sqlx::query("UPDATE streams SET checkpoint = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(batch.workspace.as_uuid())
            .bind(batch.stream_id)
            .bind(batch.checkpoint)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)?;
    }
    crate::changes::request_reevaluation(&mut tx, batch.workspace).await?;
    commit(tx).await?;
    Ok(BatchOutcome::Committed(BatchProgress {
        batch: counts,
        events: events.counts,
        run: Counts {
            read,
            applied,
            unchanged,
            rejected,
            deleted,
        },
        threshold_exceeded: exceeded,
    }))
}

/// Whether the reject share is over the run's threshold: only once at least
/// [`SYNC_REJECT_CHECK_MIN_RECORDS`] records are read.
fn threshold_exceeded(read: i64, rejected: i64, threshold: f64) -> bool {
    read >= SYNC_REJECT_CHECK_MIN_RECORDS && (rejected as f64) / (read as f64) > threshold
}

fn decision(
    batch: &SyncBatch<'_>,
    spec: &'static DecisionSpec,
    detail: Option<&'static str>,
    subject_ids: Vec<(&'static str, Uuid)>,
) -> NewDecision {
    NewDecision {
        graph: None,
        workspace_id: batch.workspace,
        trace: batch.trace.clone(),
        spec,
        detail,
        subject_ids,
        job: Some((batch.job_id, batch.attempt)),
    }
}

/// Writes the batch's offsets. A consumer is the only writer of its stream's rows while it holds
/// the lease, so the value is exact, not a maximum.
pub(crate) async fn write_offsets(
    tx: &mut crate::Tx,
    workspace: WorkspaceId,
    stream_id: Uuid,
    offsets: &[PartitionOffset],
) -> Result<(), StoreError> {
    if offsets.is_empty() {
        return Ok(());
    }
    let partitions: Vec<i32> = offsets.iter().map(|offset| offset.partition).collect();
    let nexts: Vec<i64> = offsets.iter().map(|offset| offset.next).collect();
    let ends: Vec<Option<i64>> = offsets.iter().map(|offset| offset.end).collect();
    sqlx::query(
        "INSERT INTO stream_offsets \
            (workspace_id, stream_id, partition, next_offset, high_watermark, observed_at) \
         SELECT $1, $2, t.partition, t.next_offset, t.high_watermark, \
                CASE WHEN t.high_watermark IS NULL THEN NULL ELSE now() END \
         FROM unnest($3::int4[], $4::int8[], $5::int8[]) \
              AS t(partition, next_offset, high_watermark) \
         ON CONFLICT (stream_id, partition) DO UPDATE SET \
            next_offset = EXCLUDED.next_offset, \
            high_watermark = COALESCE(EXCLUDED.high_watermark, stream_offsets.high_watermark), \
            observed_at = CASE WHEN EXCLUDED.high_watermark IS NULL \
                               THEN stream_offsets.observed_at ELSE now() END, \
            updated_at = now()",
    )
    .bind(workspace.as_uuid())
    .bind(stream_id)
    .bind(&partitions)
    .bind(&nexts)
    .bind(&ends)
    .execute(&mut **tx)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Applies the accepted records, all together or, with `singly`, each in a savepoint. A record
/// the store cannot apply is `None`.
async fn apply_upserts(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
) -> Result<Vec<Option<ProfileOutcome>>, StoreError> {
    let records: Vec<ProfileRecord> = batch
        .upserts
        .iter()
        .map(|upsert| upsert.record.clone())
        .collect();
    if batch.singly {
        return apply_profile_records_singly(tx, batch.workspace, &records).await;
    }
    let outcomes = apply_profile_records(tx, batch.workspace, &records).await?;
    Ok(outcomes.into_iter().map(Some).collect())
}

/// What became of one record's event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventResult {
    Accepted,
    Duplicate,
    Rejected,
}

/// The events of a batch, by the position of their record.
#[derive(Default)]
struct EventsApplied {
    per_upsert: Vec<Option<EventResult>>,
    /// The rejects the event path wrote or found open for these events.
    reject_ids: Vec<Uuid>,
    /// The `message_id`s of events refused as `apply_failed`, whose rejects have no id to hand
    /// back.
    failed_messages: Vec<String>,
    counts: EventCounts,
}

/// The idempotency key of a record's event: the same record, stream, source and event name give
/// the same key, so a re-read of the record never duplicates its event, and a record that
/// changed does not rewrite its past.
fn event_message_id(batch: &SyncBatch<'_>, upsert: &SyncUpsert, name: &str) -> String {
    // A message is told from the next with the same key by where it sits in the log.
    let identity = upsert
        .occurrence
        .as_deref()
        .unwrap_or(upsert.record.record_key.as_str());
    let mut hash = Sha256::new();
    for (position, part) in [batch.source, batch.stream, identity, name]
        .into_iter()
        .enumerate()
    {
        if position > 0 {
            hash.update([0]);
        }
        hash.update(part.as_bytes());
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sync:{digest}")
}

/// Applies the event of each record whose profile was written or already current, through the
/// event path: validation against the published schema, redaction and reject handling. With
/// `singly`, each event gets a savepoint of its own and one that cannot be stored is rejected as
/// `apply_failed`.
async fn apply_events(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    results: &[Option<ProfileOutcome>],
    seen: &HashMap<String, Uuid>,
) -> Result<EventsApplied, StoreError> {
    let mut applied = EventsApplied::default();
    if batch.upserts.iter().all(|upsert| upsert.event.is_none()) {
        return Ok(applied);
    }
    // The job fails a run with events and no key before it reads; a batch never gets here
    // without one.
    let key = batch.redaction_key.ok_or_else(StoreError::poison)?;
    let mut events = EventBatch::default();
    for (upsert, result) in batch.upserts.iter().zip(results) {
        let current = matches!(
            result,
            Some(ProfileOutcome::Applied { .. } | ProfileOutcome::Unchanged { .. })
        );
        let Some(event) = upsert.event.as_ref().filter(|_| current) else {
            applied.per_upsert.push(None);
            continue;
        };
        let message_id = event_message_id(batch, upsert, &event.name);
        let record = EventRecord {
            source: batch.source.to_owned(),
            identifiers: upsert.record.identifiers.clone(),
            name: event.name.clone(),
            properties: event.properties.clone(),
            timestamp: event.timestamp,
            received_at: batch.received_at,
            message_id: Some(message_id.clone()),
            trace: batch.trace.clone(),
            // The event goes with the record: removing the record removes it.
            source_record_id: seen.get(upsert.record.record_key.as_str()).copied(),
        };
        let outcome = if batch.singly {
            let outcome = apply_event_singly(tx, batch, key, &record, &message_id).await?;
            if outcome.is_none() {
                applied.failed_messages.push(message_id);
            }
            outcome
        } else {
            Some(apply_event_record(tx, batch.workspace, key, &mut events, &record).await?)
        };
        let result = match outcome {
            Some(EventOutcome::Accepted { .. }) => {
                applied.counts.accepted += 1;
                EventResult::Accepted
            }
            Some(EventOutcome::Duplicate { .. }) => {
                applied.counts.duplicate += 1;
                EventResult::Duplicate
            }
            Some(EventOutcome::Rejected { reject_id, .. }) => {
                applied.counts.rejected += 1;
                applied.reject_ids.push(reject_id);
                EventResult::Rejected
            }
            None => {
                applied.counts.rejected += 1;
                EventResult::Rejected
            }
        };
        applied.per_upsert.push(Some(result));
    }
    events.finish(tx, batch.workspace).await?;
    Ok(applied)
}

/// One event in a savepoint of its own. `None` when it cannot be stored: it is rejected as
/// `apply_failed` and the batch goes on.
async fn apply_event_singly(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    key: &HmacKey,
    record: &EventRecord,
    message_id: &str,
) -> Result<Option<EventOutcome>, StoreError> {
    let mut savepoint = tx.begin().await.map_err(StoreError::query)?;
    let mut single = EventBatch::default();
    let done =
        match apply_event_record(&mut savepoint, batch.workspace, key, &mut single, record).await {
            Ok(outcome) => single
                .finish(&mut savepoint, batch.workspace)
                .await
                .map(|()| outcome),
            Err(error) => Err(error),
        };
    match done {
        Ok(outcome) => {
            savepoint.commit().await.map_err(StoreError::query)?;
            Ok(Some(outcome))
        }
        Err(error) if error.is_retryable() => Err(error),
        Err(_) => {
            savepoint.rollback().await.map_err(StoreError::query)?;
            reject_apply_failed(
                tx,
                batch.workspace,
                key,
                &FailedItem {
                    kind: "event",
                    source: batch.source,
                    name: Some(&record.name),
                    values: &record.properties,
                    timestamp: record.timestamp,
                    received_at: batch.received_at,
                    message_id: Some(message_id),
                    trace: batch.trace,
                },
            )
            .await?;
            Ok(None)
        }
    }
}

/// Ties the rejects of the batch's events to the run, so the run's detail lists what it hit.
/// An open reject the event path found for the same message is tied to this run too.
async fn link_event_rejects(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    events: &EventsApplied,
) -> Result<(), StoreError> {
    if events.reject_ids.is_empty() && events.failed_messages.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE ingest_rejects SET sync_run_id = $2 \
         WHERE workspace_id = $1 \
           AND (id = ANY($3) OR (message_id = ANY($4) AND replayed_at IS NULL))",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.run_id)
    .bind(&events.reject_ids)
    .bind(&events.failed_messages)
    .execute(&mut **tx)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Adds the people of the records whose profile was written or already current to the lists the
/// mapping names. False when a list does not exist: the caller rolls the batch back.
async fn join_lists(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    results: &[Option<ProfileOutcome>],
) -> Result<bool, StoreError> {
    // Lists in one order, so two runs never wait on each other's lists.
    let mut by_list: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (upsert, result) in batch.upserts.iter().zip(results) {
        let Some(
            ProfileOutcome::Applied { person_id, .. } | ProfileOutcome::Unchanged { person_id, .. },
        ) = result
        else {
            continue;
        };
        for list in &upsert.lists {
            by_list.entry(*list).or_default().push(*person_id);
        }
    }
    for (list, mut people) in by_list {
        people.sort_unstable();
        people.dedup();
        for chunk in people.chunks(MAX_PEOPLE_PER_CALL) {
            match add_members(
                tx,
                batch.workspace,
                list,
                chunk,
                ListSource::Sync,
                None,
                batch.trace,
            )
            .await
            {
                Ok(_) => {}
                Err(MembershipError::NoSuchList) => return Ok(false),
                Err(MembershipError::Store(error)) => return Err(error),
                Err(MembershipError::TooMany) => return Err(StoreError::poison()),
            }
        }
    }
    Ok(true)
}

/// Sets `last_seen_run` on the live ledger rows of `keys` and returns their ids by key.
async fn mark_seen(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    keys: &[&str],
) -> Result<HashMap<String, Uuid>, StoreError> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "UPDATE source_records SET last_seen_run = $5 \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = ANY($4) \
           AND deleted_at IS NULL \
         RETURNING record_key, id",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.source)
    .bind(batch.stream)
    .bind(keys)
    .bind(batch.run_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::super::model::{cancel_run, find_run};
    use super::super::testing::{Fixture, batch, fixture, upsert};
    use super::*;

    type DecisionTuple = (
        uuid::Uuid,
        String,
        String,
        String,
        String,
        Option<String>,
        Value,
    );
    type RejectTuple = (
        uuid::Uuid,
        String,
        Option<String>,
        Option<uuid::Uuid>,
        String,
    );

    fn reject() -> SyncReject {
        SyncReject {
            record_key: "rej".into(),
            reasons: vec![("email".into(), "invalid_identifier")],
            sample: json!({"properties": {"email": null}, "masked": ["email"]}),
        }
    }

    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    /// The run's counters, checkpoint and state, and how much was written around it.
    async fn nothing_written(pool: &PgPool, fx: &Fixture) {
        let run = find_run(pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                run.records_read,
                run.records_applied,
                run.records_rejected,
                run.records_deleted
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(run.checkpoint, None);
        assert_eq!(count(pool, "SELECT count(*) FROM source_records").await, 0);
        assert_eq!(count(pool, "SELECT count(*) FROM ingest_rejects").await, 0);
        assert_eq!(
            count(pool, "SELECT count(*) FROM decision_records").await,
            0
        );
        assert_eq!(count(pool, "SELECT count(*) FROM people").await, 0);
    }

    #[test]
    fn the_threshold_applies_only_from_the_minimum_record_count() {
        assert!(!threshold_exceeded(99, 99, 0.05));
        assert!(threshold_exceeded(100, 6, 0.05));
        assert!(!threshold_exceeded(100, 5, 0.05));
        assert!(!threshold_exceeded(1000, 50, 0.05));
        assert!(threshold_exceeded(1000, 51, 0.05));
        assert!(!threshold_exceeded(100, 100, 1.0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_applies_deletes_and_rejects_and_records_each_under_the_run_trace(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let first = [
            upsert(&fx, "k0", "a0@example.com"),
            upsert(&fx, "k1", "a1@example.com"),
            upsert(&fx, "k2", "a2@example.com"),
        ];
        let cp1 = json!({"offset": 3});
        let one = commit_sync_batch(&pool, &batch(&fx, &source, &first, &cp1))
            .await
            .unwrap();
        let BatchOutcome::Committed(one) = one else {
            panic!("{one:?}");
        };
        assert_eq!((one.batch.read, one.batch.applied), (3, 3));

        let second = [
            upsert(&fx, "k3", "a3@example.com"),
            upsert(&fx, "k1", "a1@example.com"),
            upsert(&fx, "kbad", "not-an-email"),
        ];
        let deleted = [("k0".to_owned(), DELETE_BY_SOURCE)];
        let rejected = [reject()];
        let cp2 = json!({"offset": 6});
        let two = commit_sync_batch(
            &pool,
            &SyncBatch {
                deleted: &deleted,
                rejected: &rejected,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(two) = two else {
            panic!("{two:?}");
        };

        assert_eq!(
            (
                two.batch.read,
                two.batch.applied,
                two.batch.unchanged,
                two.batch.rejected,
                two.batch.deleted
            ),
            (5, 1, 1, 2, 1)
        );
        assert_eq!(
            two.run,
            Counts {
                read: 8,
                applied: 4,
                unchanged: 1,
                rejected: 2,
                deleted: 1
            }
        );
        assert!(!two.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.checkpoint, Some(cp2));
        assert_eq!(run.state, "running");

        // Every decision of the run is under its one trace, caused by the run.
        let decisions: Vec<DecisionTuple> = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, action, outcome, reason, subject_ids \
                 FROM decision_records ORDER BY recorded_at, id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!decisions.is_empty());
        for decision in &decisions {
            assert_eq!(decision.0, fx.run.trace_id);
            assert_eq!(decision.1, "sync_run");
            assert_eq!(decision.2, fx.run.id.to_string());
        }
        let of = |action: &str, outcome: &str| {
            decisions
                .iter()
                .filter(|d| d.3 == action && d.4 == outcome)
                .collect::<Vec<_>>()
        };
        let applied = of("sync.record", "applied");
        assert_eq!(applied.len(), 4);
        for decision in &applied {
            assert!(decision.6["person"].is_string() && decision.6["source_record"].is_string());
        }
        let rejects = of("sync.record", "rejected");
        assert_eq!(rejects.len(), 2);
        let reject_ids: Vec<String> = sqlx::query_scalar("SELECT id::text FROM ingest_rejects")
            .fetch_all(&pool)
            .await
            .unwrap();
        for decision in &rejects {
            let id = decision.6["ingest_reject"].as_str().unwrap();
            assert!(reject_ids.iter().any(|known| known == id));
            assert_eq!(decision.6.as_object().unwrap().len(), 1);
        }
        let mut codes: Vec<_> = rejects.iter().map(|d| d.5.clone().unwrap()).collect();
        codes.sort();
        assert_eq!(codes, ["invalid_email", "invalid_identifier"]);
        assert_eq!(of("contribution.removed", "removed").len(), 1);
        let shown = format!("{decisions:?}");
        assert!(!shown.contains("example.com") && !shown.contains("not-an-email"));

        // The rejects name the run and hold no person.
        let rows: Vec<RejectTuple> =
            sqlx::query_as("SELECT sync_run_id, kind, name, person_id, source FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.0, fx.run.id);
            assert_eq!(row.1, "record");
            assert_eq!(row.2.as_deref(), Some("contacts"));
            assert_eq!(row.3, None);
            assert_eq!(row.4, source);
        }
        // Applied and unchanged records are marked seen by the run; a full refresh keeps no
        // cursor on the stream.
        let seen: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT record_key, last_seen_run FROM source_records \
             WHERE deleted_at IS NULL ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let keys: Vec<_> = seen.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["k1", "k2", "k3"]);
        assert!(seen.iter().all(|(_, run)| *run == Some(fx.run.id)));
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_fails_midway_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        // PostgreSQL refuses a NUL in jsonb, after the record was applied and before the commit.
        let poison = [SyncReject {
            record_key: "poison".into(),
            reasons: vec![("\u{0}".into(), "invalid_identifier")],
            sample: json!({}),
        }];
        let cp = json!({"offset": 1});

        let result = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &poison,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await;

        assert!(result.is_err());
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_whose_lease_was_taken_over_commits_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::LeaseLost));
        nothing_written(&pool, &fx).await;
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_for_a_job_that_is_gone_commits_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &upserts, &cp))
            .await
            .unwrap();

        assert!(matches!(outcome, BatchOutcome::LeaseLost));
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancel_stops_the_run_at_the_batch_boundary(pool: PgPool) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let first = [upsert(&fx, "k0", "a0@example.com")];
        let cp1 = json!({"offset": 1});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &first, &cp1)
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, BatchOutcome::Committed(_)));
        let mut connection = pool.acquire().await.unwrap();
        cancel_run(&mut connection, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap();
        drop(connection);

        let second = [upsert(&fx, "k1", "a1@example.com")];
        let cp2 = json!({"offset": 2});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::Cancelled));
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "cancelled");
        assert_eq!((run.records_read, run.records_applied), (1, 1));
        assert_eq!(run.checkpoint, Some(cp1.clone()));
        let keys: Vec<String> = sqlx::query_scalar("SELECT record_key FROM source_records")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(keys, ["k0"]);
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, Some(cp1));
    }

    fn many(fx: &Fixture, from: usize, to: usize) -> Vec<SyncUpsert> {
        (from..to)
            .map(|n| upsert(fx, &format!("k{n}"), &format!("a{n}@example.com")))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dropped_duplicate_counts_as_read_and_in_the_reject_check_only(pool: PgPool) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        // 90 applied, 6 rejected and 4 duplicates dropped by the planner: 100 records read, and
        // 6 of them rejected is over the 5% threshold. Without the duplicates only 96 were read
        // and the check would not apply yet.
        let upserts = many(&fx, 0, 90);
        let rejects: Vec<SyncReject> = (0..6).map(|_| reject()).collect();
        let cp = json!({"offset": 100});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejects,
                duplicates: 4,
                incremental: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(progress.batch.read, 100);
        assert_eq!(
            (
                progress.batch.applied,
                progress.batch.unchanged,
                progress.batch.rejected,
                progress.batch.deleted
            ),
            (90, 0, 6, 0)
        );
        assert!(progress.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.records_read, 100);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reject_check_waits_for_the_minimum_then_fails_the_run_and_keeps_earlier_batches(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();

        // 10 applied and 89 rejected: 99 records read, over the share but under the minimum.
        let first = many(&fx, 0, 10);
        let rejects: Vec<SyncReject> = (0..89).map(|_| reject()).collect();
        let cp1 = json!({"offset": 99});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejects,
                incremental: true,
                ..batch(&fx, &source, &first, &cp1)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(progress.run.read, 99);
        assert!(!progress.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "running");

        // The hundredth record, a reject, puts it over.
        let second = many(&fx, 10, 11);
        let one = [reject()];
        let cp2 = json!({"offset": 100});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &one,
                incremental: true,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };

        assert!(progress.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "failed");
        assert_eq!(run.error_code.as_deref(), Some("reject_threshold"));
        assert!(run.finished_at.is_some());
        // Earlier batches stay applied; nothing is swept.
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM source_records WHERE deleted_at IS NULL"
            )
            .await,
            11
        );
        // The stream's cursor stays at the last batch that did not fail the run.
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, Some(cp1));
        // A failed run takes no more batches.
        let late = many(&fx, 11, 12);
        let cp3 = json!({"offset": 101});
        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &late, &cp3))
            .await
            .unwrap();
        assert!(matches!(outcome, BatchOutcome::Cancelled));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_incremental_run_moves_the_streams_checkpoint(pool: PgPool) {
        let incremental = fixture(&pool, "one", "incremental").await;
        let full = fixture(&pool, "two", "full_refresh").await;
        for (fx, is_incremental) in [(&incremental, true), (&full, false)] {
            let source = fx.source_id.to_string();
            let upserts = [upsert(fx, "k0", "a0@example.com")];
            let cp = json!({"offset": 1});
            let outcome = commit_sync_batch(
                &pool,
                &SyncBatch {
                    incremental: is_incremental,
                    ..batch(fx, &source, &upserts, &cp)
                },
            )
            .await
            .unwrap();
            assert!(matches!(outcome, BatchOutcome::Committed(_)));
        }

        for (fx, expected) in [(&incremental, Some(json!({"offset": 1}))), (&full, None)] {
            let stream: Option<Value> =
                sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                    .bind(fx.stream_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stream, expected);
            let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.checkpoint, Some(json!({"offset": 1})));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn applying_record_by_record_gives_the_same_result(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [
            upsert(&fx, "k0", "a0@example.com"),
            upsert(&fx, "kbad", "not-an-email"),
        ];
        let cp = json!({"offset": 2});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                singly: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.applied, progress.batch.rejected), (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_nul_in_a_record_does_not_stop_its_reject_from_being_stored(pool: PgPool) {
        use zirv_flow_core::mapping::{ColumnRule, Mapping, Target, reject_sample};

        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let mapping = Mapping {
            delete_when: None,
            key_column: None,
            event: None,
            columns: vec![ColumnRule {
                column: "plan".into(),
                target: Target::Trait { key: "plan".into() },
                transforms: Vec::new(),
            }],
        };
        let data = json!({"plan": "pr\0o", "e\0mail": "a@b.c"});
        let sample = reject_sample(&mapping, &data, |_| true);
        let rejected = [SyncReject {
            record_key: "kwrong".into(),
            reasons: vec![("plan".into(), "wrong_type")],
            sample: json!({"properties": sample.properties, "masked": sample.masked}),
        }];
        let cp = json!({"offset": 1});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejected,
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::Committed(_)));
        let stored: Value = sqlx::query_scalar("SELECT sample FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored["properties"]["plan"], "pr\u{FFFD}o");
        assert_eq!(stored["masked"], json!(["e\u{FFFD}mail"]));
    }

    // The events and lists of a batch (file sources).

    fn key() -> zirv_flow_core::HmacKey {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
    }

    /// A record with an `order_placed` event carrying `total`, joining `lists`.
    fn with_event(
        fx: &Fixture,
        record_key: &str,
        email: &str,
        total: Value,
        lists: &[Uuid],
    ) -> SyncUpsert {
        let mut item = upsert(fx, record_key, email);
        item.lists = lists.to_vec();
        item.event = Some(SyncEvent {
            name: "order_placed".into(),
            properties: json!({ "total": total }).as_object().cloned().unwrap(),
            timestamp: None,
        });
        item
    }

    /// Publishes `order_placed` in block mode, with a required numeric `total`.
    async fn block_order_placed(pool: &PgPool, fx: &Fixture) {
        use crate::events::{NewEventSchema, create_event_schema, publish_schema_draft};
        use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};

        create_event_schema(
            pool,
            fx.ws,
            &NewEventSchema {
                name: "order_placed".into(),
                mode: EventMode::Block,
                description: String::new(),
                properties: vec![SchemaProperty {
                    name: "total".into(),
                    property_type: PropertyType::Number,
                    required: true,
                    enum_values: None,
                    description: String::new(),
                    pii: false,
                }],
            },
        )
        .await
        .unwrap();
        let mut tx = begin(pool).await.unwrap();
        publish_schema_draft(&mut tx, fx.ws, "order_placed", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    async fn list(pool: &PgPool, fx: &Fixture, name: &str) -> Uuid {
        crate::create_list(pool, fx.ws, name, "", None)
            .await
            .unwrap()
            .id
    }

    /// A batch of the fixture carrying the redaction key.
    fn keyed<'a>(
        fx: &'a Fixture,
        source: &'a str,
        upserts: &'a [SyncUpsert],
        checkpoint: &'a Value,
        key: &'a zirv_flow_core::HmacKey,
    ) -> SyncBatch<'a> {
        SyncBatch {
            redaction_key: Some(key),
            ..batch(fx, source, upserts, checkpoint)
        }
    }

    async fn nothing_of_events_and_lists(pool: &PgPool) {
        for (table, sql) in [
            ("events", "SELECT count(*) FROM events"),
            ("list_members", "SELECT count(*) FROM list_members"),
            (
                "list_member_changes",
                "SELECT count(*) FROM list_member_changes",
            ),
            ("ingest_rejects", "SELECT count(*) FROM ingest_rejects"),
            ("people", "SELECT count(*) FROM people"),
            ("source_records", "SELECT count(*) FROM source_records"),
        ] {
            assert_eq!(count(pool, sql).await, 0, "{table}");
        }
    }

    #[test]
    fn an_events_message_id_is_stable_sixty_nine_characters_and_differs_by_every_part() {
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("test"));
        let at = OffsetDateTime::now_utc();
        let checkpoint = json!({});
        let make = |source: &'static str, stream: &'static str| SyncBatch {
            workspace: zirv_flow_core::WorkspaceId::from_uuid(Uuid::nil()),
            run_id: Uuid::nil(),
            stream_id: Uuid::nil(),
            source,
            stream,
            job_id: Uuid::nil(),
            lease_token: Uuid::nil(),
            attempt: 1,
            trace: &trace,
            received_at: at,
            redaction_key: None,
            upserts: &[],
            deleted: &[],
            seen: &[],
            rejected: &[],
            duplicates: 0,
            offsets: &[],
            checkpoint: &checkpoint,
            incremental: false,
            singly: false,
        };
        let up = |key: &str, occurrence: Option<&str>| SyncUpsert {
            record: ProfileRecord {
                source: "x".into(),
                stream: "t".into(),
                record_key: key.into(),
                identifiers: Vec::new(),
                traits: Map::new(),
                observed_at: at,
                trace: trace.clone(),
            },
            sample: json!({}),
            lists: Vec::new(),
            event: None,
            occurrence: occurrence.map(str::to_owned),
        };
        let id = |batch: &SyncBatch<'_>, key: &str, name: &str| {
            event_message_id(batch, &up(key, None), name)
        };

        let base = id(&make("s", "t"), "k", "e");

        assert_eq!(base.len(), 69);
        assert!(base.starts_with("sync:"));
        assert_eq!(base, id(&make("s", "t"), "k", "e"));
        for other in [
            id(&make("s2", "t"), "k", "e"),
            id(&make("s", "t2"), "k", "e"),
            id(&make("s", "t"), "k2", "e"),
            id(&make("s", "t"), "k", "e2"),
            // The parts are separated, not concatenated.
            id(&make("s", "t"), "ke", ""),
            // A message is told apart by its place in the log, not by its key.
            event_message_id(&make("s", "t"), &up("k", Some("0/5")), "e"),
        ] {
            assert_ne!(base, other);
        }
        let batch = make("s", "t");
        assert_ne!(
            event_message_id(&batch, &up("k", Some("0/5")), "e"),
            event_message_id(&batch, &up("k", Some("0/6")), "e"),
            "two messages with one key are two events"
        );
        assert_eq!(
            event_message_id(&batch, &up("k", Some("0/5")), "e"),
            event_message_id(&batch, &up("other", Some("0/5")), "e"),
            "a replay of the same offset is the same event"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_applies_profiles_events_and_list_members_in_one_transaction(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let vip = list(&pool, &fx, "vip").await;
        let key = key();
        let upserts = [
            with_event(&fx, "k0", "a0@example.com", json!(10), &[news]),
            with_event(&fx, "k1", "a1@example.com", json!(20), &[news, vip]),
            upsert(&fx, "k2", "a2@example.com"),
        ];
        let cp = json!({"offset": 3});

        let outcome = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.read, progress.batch.applied), (3, 3));
        assert_eq!(
            progress.events,
            EventCounts {
                accepted: 2,
                duplicate: 0,
                rejected: 0
            }
        );
        // The events are the people's, under the run's trace, from the source, at the run's time.
        let events: Vec<(String, String, Uuid, Value, Option<String>)> = sqlx::query_as(
            "SELECT name, source, trace_id, properties, message_id FROM events ORDER BY received_at, id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(events.len(), 2);
        for event in &events {
            assert_eq!(event.0, "order_placed");
            assert_eq!(event.1, source);
            assert_eq!(event.2, fx.run.trace_id);
            assert!(event.4.as_deref().unwrap().starts_with("sync:"));
        }
        let received: Vec<OffsetDateTime> = sqlx::query_scalar("SELECT received_at FROM events")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(received.iter().all(|at| *at == received[0]));
        // The members: two in news, one in vip, all from a sync, with a history row each.
        let members: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT list_id, source FROM list_members ORDER BY list_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|(_, source)| source == "sync"));
        let changes: Vec<(String, String)> =
            sqlx::query_as("SELECT action, source FROM list_member_changes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(changes.len(), 3);
        assert!(
            changes
                .iter()
                .all(|(action, source)| action == "added" && source == "sync")
        );
        let counts: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id, member_count FROM lists")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(counts.iter().find(|(id, _)| *id == news).unwrap().1, 2);
        assert_eq!(counts.iter().find(|(id, _)| *id == vip).unwrap().1, 1);
        // Every decision of the batch is under the run's trace and caused by the run, and none
        // holds a value.
        let decisions: Vec<(Uuid, String, String, String, Value)> = sqlx::query_as(
            "SELECT trace_id, cause_kind, action, outcome, subject_ids FROM decision_records",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for decision in &decisions {
            assert_eq!(decision.0, fx.run.trace_id);
            assert_eq!(decision.1, "sync_run");
        }
        let actions: std::collections::BTreeSet<_> =
            decisions.iter().map(|d| d.2.as_str()).collect();
        assert!(actions.contains("event.accepted") && actions.contains("list.membership"));
        assert!(actions.contains("sync.record"));
        let shown = format!("{decisions:?}");
        assert!(!shown.contains("example.com") && !shown.contains("order_placed"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_read_of_the_same_records_changes_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let key = key();
        let upserts = [
            with_event(&fx, "k0", "a0@example.com", json!(10), &[news]),
            with_event(&fx, "k1", "a1@example.com", json!(20), &[news]),
        ];
        let cp = json!({"offset": 2});
        commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();
        let events_before = count(&pool, "SELECT count(*) FROM events").await;
        let changes_before = count(&pool, "SELECT count(*) FROM list_member_changes").await;

        let again = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();

        let BatchOutcome::Committed(progress) = again else {
            panic!("{again:?}");
        };
        assert_eq!(
            (
                progress.batch.applied,
                progress.batch.unchanged,
                progress.batch.rejected
            ),
            (0, 2, 0)
        );
        assert_eq!(
            progress.events,
            EventCounts {
                accepted: 0,
                duplicate: 2,
                rejected: 0
            }
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM events").await,
            events_before
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM list_member_changes").await,
            changes_before
        );
        let duplicates: i64 =
            sqlx::query_scalar("SELECT sum(duplicates)::bigint FROM event_dedupe")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(duplicates, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_event_on_an_unchanged_profile_counts_the_record_as_applied(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let key = key();
        let first = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        commit_sync_batch(&pool, &keyed(&fx, &source, &first, &cp, &key))
            .await
            .unwrap();

        let second = [with_event(&fx, "k0", "a0@example.com", json!(5), &[])];
        let outcome = commit_sync_batch(&pool, &keyed(&fx, &source, &second, &cp, &key))
            .await
            .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            (progress.batch.applied, progress.batch.unchanged),
            (1, 0),
            "an accepted event is something the record changed"
        );
        assert_eq!(progress.events.accepted, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_refused_event_is_a_reject_of_the_run_and_the_record_counts_once_as_rejected(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        block_order_placed(&pool, &fx).await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let key = key();
        let upserts = [
            with_event(&fx, "good", "a0@example.com", json!(10), &[news]),
            with_event(&fx, "bad", "a1@example.com", json!("not a number"), &[news]),
        ];
        let cp = json!({"offset": 2});

        let outcome = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        // One accepted, one refused: the refused record is rejected, its profile change stands.
        assert_eq!(
            (
                progress.batch.read,
                progress.batch.applied,
                progress.batch.rejected
            ),
            (2, 1, 1)
        );
        assert_eq!((progress.events.accepted, progress.events.rejected), (1, 1));
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 2);
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM list_members").await, 2);
        // The reject is the event path's, kind event, tied to the run, with no value in it.
        let rejects: Vec<(String, Option<Uuid>, Value, Option<String>)> =
            sqlx::query_as("SELECT kind, sync_run_id, sample, reason FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rejects.len(), 1);
        assert_eq!(rejects[0].0, "event");
        assert_eq!(rejects[0].1, Some(fx.run.id));
        assert_eq!(rejects[0].3.as_deref(), Some("validation_failed"));
        let listed = crate::list_run_rejects(&pool, fx.ws, fx.run.id, 10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (run.records_read, run.records_applied, run.records_rejected),
            (2, 1, 1)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_open_reject_found_again_is_tied_to_the_run_that_hit_it(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        block_order_placed(&pool, &fx).await;
        let source = fx.source_id.to_string();
        let key = key();
        let upserts = [with_event(&fx, "bad", "a1@example.com", json!("x"), &[])];
        let cp = json!({"offset": 1});
        commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();
        // The reject stays open and is forgotten by its run, as when an earlier run is deleted.
        sqlx::query("UPDATE ingest_rejects SET sync_run_id = NULL")
            .execute(&pool)
            .await
            .unwrap();

        let again = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();

        let BatchOutcome::Committed(progress) = again else {
            panic!("{again:?}");
        };
        assert_eq!(progress.batch.rejected, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM ingest_rejects").await, 1);
        let linked: Option<Uuid> = sqlx::query_scalar("SELECT sync_run_id FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(linked, Some(fx.run.id));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_missing_list_rolls_the_whole_batch_back(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let key = key();
        let upserts = [with_event(
            &fx,
            "k0",
            "a0@example.com",
            json!(1),
            &[Uuid::now_v7()],
        )];
        let cp = json!({"offset": 1});

        let outcome = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();

        assert!(matches!(outcome, BatchOutcome::ListMissing));
        nothing_of_events_and_lists(&pool).await;
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_the_events_and_members_rolls_back_profiles_events_and_members_together(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let key = key();
        let upserts = [with_event(&fx, "k0", "a0@example.com", json!(1), &[news])];
        // PostgreSQL refuses a NUL in jsonb, after everything else was written.
        let poison = [SyncReject {
            record_key: "poison".into(),
            reasons: vec![("\u{0}".into(), "invalid_identifier")],
            sample: json!({}),
        }];
        let cp = json!({"offset": 1});

        let result = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &poison,
                ..keyed(&fx, &source, &upserts, &cp, &key)
            },
        )
        .await;

        assert!(result.is_err());
        nothing_of_events_and_lists(&pool).await;
        assert_eq!(
            count(&pool, "SELECT count(*) FROM event_dedupe").await,
            0,
            "the dedupe row is rolled back with the event"
        );
        let members: i64 = sqlx::query_scalar("SELECT member_count FROM lists WHERE id = $1")
            .bind(news)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(members, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_of_events_and_members_is_fenced_by_the_lease_and_by_a_cancel(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let key = key();
        let upserts = [with_event(&fx, "k0", "a0@example.com", json!(1), &[news])];
        let cp = json!({"offset": 1});

        // Another worker holds the lease.
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();
        let lost = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();
        assert!(matches!(lost, BatchOutcome::LeaseLost));
        nothing_of_events_and_lists(&pool).await;

        // The run was cancelled.
        let fx = fixture(&pool, "two", "full_refresh").await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let upserts = [with_event(&fx, "k0", "a0@example.com", json!(1), &[news])];
        let mut connection = pool.acquire().await.unwrap();
        cancel_run(&mut connection, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap();
        drop(connection);
        let cancelled = commit_sync_batch(&pool, &keyed(&fx, &source, &upserts, &cp, &key))
            .await
            .unwrap();
        assert!(matches!(cancelled, BatchOutcome::Cancelled));
        nothing_of_events_and_lists(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn applying_events_record_by_record_gives_the_same_result(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        block_order_placed(&pool, &fx).await;
        let source = fx.source_id.to_string();
        let news = list(&pool, &fx, "news").await;
        let key = key();
        let upserts = [
            with_event(&fx, "good", "a0@example.com", json!(10), &[news]),
            with_event(&fx, "bad", "a1@example.com", json!("x"), &[news]),
        ];
        let cp = json!({"offset": 2});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                singly: true,
                ..keyed(&fx, &source, &upserts, &cp, &key)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.applied, progress.batch.rejected), (1, 1));
        assert_eq!((progress.events.accepted, progress.events.rejected), (1, 1));
        assert_eq!(count(&pool, "SELECT count(*) FROM list_members").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_with_events_and_no_redaction_key_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [with_event(&fx, "k0", "a0@example.com", json!(1), &[])];
        let cp = json!({"offset": 1});

        let result = commit_sync_batch(&pool, &batch(&fx, &source, &upserts, &cp)).await;

        assert!(result.is_err());
        nothing_of_events_and_lists(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_failed_on_its_rejects_marks_its_stream_failing_in_the_same_transaction(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let rejects: Vec<SyncReject> = (0..SYNC_REJECT_CHECK_MIN_RECORDS)
            .map(|_| reject())
            .collect();
        let cp = json!({"offset": 100});
        let before = crate::find_stream_by_id(&pool, fx.ws, fx.source_id, fx.stream_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!before.last_run_failed);

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejects,
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(progress.threshold_exceeded);
        let after = crate::find_stream_by_id(&pool, fx.ws, fx.source_id, fx.stream_id)
            .await
            .unwrap()
            .unwrap();
        assert!(after.last_run_failed);
        assert_eq!(after.last_success_at, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_removed_record_appends_a_deleted_decision_with_the_reason_it_was_removed_for(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let first = many(&fx, 0, 3);
        let cp1 = json!({"offset": 3});
        commit_sync_batch(&pool, &batch(&fx, &source, &first, &cp1))
            .await
            .unwrap();

        let deleted = [
            ("k0".to_owned(), DELETE_BY_SOURCE),
            ("k1".to_owned(), DELETE_BY_SIGNAL),
            ("ghost".to_owned(), DELETE_BY_SOURCE),
        ];
        let cp2 = json!({"offset": 6});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                deleted: &deleted,
                ..batch(&fx, &source, &[], &cp2)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        // The key that has no live record is unchanged, as before.
        assert_eq!((progress.batch.deleted, progress.batch.unchanged), (2, 1));
        let rows: Vec<(String, Option<String>, Value, uuid::Uuid)> = sqlx::query_as(
            "SELECT outcome, reason, subject_ids, trace_id FROM decision_records \
             WHERE action = 'sync.record' AND outcome = 'deleted' ORDER BY reason",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let reasons: Vec<_> = rows.iter().map(|row| row.1.as_deref()).collect();
        assert_eq!(reasons, [Some("signal"), Some("source")]);
        for row in &rows {
            assert_eq!(row.2.as_object().unwrap().len(), 1);
            assert!(row.2["source_record"].is_string());
            assert_eq!(row.3, fx.run.trace_id);
        }
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed'"
            )
            .await,
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_flagged_deleted_is_not_marked_seen_and_listed_keys_are_counted_as_read(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let first = many(&fx, 0, 2);
        let cp1 = json!({"offset": 2});
        commit_sync_batch(&pool, &batch(&fx, &source, &first, &cp1))
            .await
            .unwrap();
        // A second attempt of the same run: k0 is listed, k1 is deleted.
        let listed = ["k0".to_owned()];
        let deleted = [("k1".to_owned(), DELETE_BY_SOURCE)];
        let cp2 = json!({"offset": 4});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                seen: &listed,
                deleted: &deleted,
                ..batch(&fx, &source, &[], &cp2)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.read, progress.batch.deleted), (2, 1));
        assert_eq!(progress.batch.applied, 0);
        let live: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT record_key, last_seen_run FROM source_records WHERE deleted_at IS NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(live, [("k0".to_owned(), Some(fx.run.id))]);
    }

    // The offsets of a consumer (Kafka).

    fn at(partition: i32, next: i64, end: i64) -> PartitionOffset {
        PartitionOffset {
            partition,
            next,
            end: Some(end),
        }
    }

    async fn stored_offsets(pool: &PgPool) -> Vec<(i32, i64, Option<i64>)> {
        sqlx::query_as(
            "SELECT partition, next_offset, high_watermark FROM stream_offsets ORDER BY partition",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_offsets_commit_with_the_batch_and_each_batch_sets_them_exactly(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;
        let source = fx.source_id.to_string();
        let first = many(&fx, 0, 3);
        let offsets = [at(0, 2, 9), at(1, 1, 4)];
        let cp = json!({});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &offsets,
                ..batch(&fx, &source, &first, &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.read, progress.batch.applied), (3, 3));
        assert_eq!(
            stored_offsets(&pool).await,
            [(0, 2, Some(9)), (1, 1, Some(4))]
        );
        // The next batch sets the position exactly: the lease holder is the only writer.
        let again = [at(0, 5, 9)];
        commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &again,
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            stored_offsets(&pool).await,
            [(0, 5, Some(9)), (1, 1, Some(4))]
        );
        // The cursor of the stream is never written for a stream-mode run.
        let cursor: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cursor, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_is_not_committed_writes_no_offset(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let offsets = [at(0, 1, 1)];
        let cp = json!({});

        // 1. A failure after the offsets were written rolls them back with the batch.
        let poison = [SyncReject {
            record_key: "poison".into(),
            reasons: vec![("\u{0}".into(), "invalid_identifier")],
            sample: json!({}),
        }];
        let failed = commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &offsets,
                rejected: &poison,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await;
        assert!(failed.is_err());
        assert!(stored_offsets(&pool).await.is_empty(), "rolled back");

        // 2. A missing list refuses the batch after the offsets were written.
        let key = key();
        let listed = [with_event(
            &fx,
            "k1",
            "a1@example.com",
            json!(1),
            &[Uuid::now_v7()],
        )];
        let missing = commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &offsets,
                ..keyed(&fx, &source, &listed, &cp, &key)
            },
        )
        .await
        .unwrap();
        assert!(matches!(missing, BatchOutcome::ListMissing));
        assert!(stored_offsets(&pool).await.is_empty());

        // 3. A cancelled run is fenced before anything is written.
        cancel_run(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            fx.run.id,
        )
        .await
        .unwrap();
        let cancelled = commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &offsets,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();
        assert!(matches!(cancelled, BatchOutcome::Cancelled));
        assert!(stored_offsets(&pool).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_from_a_lost_lease_writes_no_offset(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let offsets = [at(0, 1, 1)];
        let cp = json!({});
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &offsets,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::LeaseLost));
        assert!(stored_offsets(&pool).await.is_empty());
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_messages_with_one_key_are_two_events_and_a_replayed_offset_is_a_duplicate(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "stream").await;
        let source = fx.source_id.to_string();
        let key = key();
        let cp = json!({});
        let commit = |occurrence: &'static str, total: i64| {
            let mut item = with_event(&fx, "same-key", "a@example.com", json!(total), &[]);
            item.occurrence = Some(occurrence.to_owned());
            let (pool, source, key, fx, cp) = (&pool, &source, &key, &fx, &cp);
            async move {
                let upserts = [item];
                let outcome = commit_sync_batch(pool, &keyed(fx, source, &upserts, cp, key))
                    .await
                    .unwrap();
                let BatchOutcome::Committed(progress) = outcome else {
                    panic!("{outcome:?}");
                };
                progress.events
            }
        };

        let first = commit("0/1", 10).await;
        let second = commit("0/2", 20).await;
        let replay = commit("0/1", 10).await;

        assert_eq!(first.accepted, 1);
        assert_eq!(second.accepted, 1, "the later message is a new event");
        assert_eq!((replay.accepted, replay.duplicate), (0, 1));
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_tombstone_removes_the_record_and_its_events_and_one_never_seen_is_unchanged(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "stream").await;
        let source = fx.source_id.to_string();
        let key = key();
        let mut item = with_event(&fx, "gone", "a@example.com", json!(1), &[]);
        item.occurrence = Some("0/0".into());
        let cp = json!({});
        commit_sync_batch(&pool, &keyed(&fx, &source, &[item], &cp, &key))
            .await
            .unwrap();
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 1);

        let deleted = [
            ("gone".to_owned(), DELETE_BY_SOURCE),
            ("never".to_owned(), DELETE_BY_SOURCE),
        ];
        let offsets = [at(0, 3, 3)];
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                deleted: &deleted,
                offsets: &offsets,
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.deleted, progress.batch.unchanged), (1, 1));
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 0);
        assert_eq!(stored_offsets(&pool).await, [(0, 3, Some(3))]);
    }
}
