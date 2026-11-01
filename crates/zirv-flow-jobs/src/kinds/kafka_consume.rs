//! `kafka.consume`: one time slice of the consumer of a stream-mode stream (architecture 7.1,
//! 7.6 and 14).
//!
//! A consumer run is `running` for days and is worked by a chain of these jobs. A slice reads the
//! topic for [`KAFKA_SLICE_BUDGET`], gathers messages and commits them in ONE transaction of
//! their own per unit (500 messages or 2 seconds): the records, the retractions, the rejects,
//! the decisions and the per-partition offsets, fenced on the job's lease and on the run's state.
//! A crash or a lost lease therefore re-reads exactly the uncommitted messages, and each
//! committed message applies once. At the end of the slice the job reports lag (which decides
//! the stream's freshness) and enqueues its successor, in the transaction that completes it.
//!
//! The successor's unique key carries the next generation: the running job still holds its own,
//! and the unique index covers running rows.
//!
//! Nothing here logs a record value, a key, a broker or a credential.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tracing::{info, warn};
use zirv_flow_connectors::sources::{
    Batch, Checkpoint, PartitionPosition, Positions, ReadPlan, Record, Refused, SourceError,
    SyncMode,
};
use zirv_flow_store::{
    LagReport, NewJob, OffsetsStored, PartitionOffset, PgPool, RunEnd, SYNC_CONSUME_JOB_KIND,
    SyncRotated, commit_stream_offsets, fail_run, load_offsets, report_stream_lag,
    rotate_stream_run, run_trace, start_stream_run, sync_consume_unique_key,
};

use super::import_rows::Probe;
use super::sync_run::{
    Failure, Gathered, Ids, LEASE_LOST, Pass, Prep, Prepared, Unit, end_failed, id_in,
    open_credential, prepare, source_failure,
};
use crate::metrics::{RunOutcome, record_sync_run};
use crate::{JobContext, JobError, JobKind};

/// How long a slice reads. Below the usual shutdown wait: a handler cannot see the stop signal,
/// so a shorter slice means a quicker clean stop.
pub(crate) const KAFKA_SLICE_BUDGET: Duration = Duration::from_secs(60);

/// A unit commits once it holds this many messages ...
const COMMIT_MESSAGES: usize = 500;

/// ... or this long after its first message, whichever comes first.
const COMMIT_AFTER: Duration = Duration::from_secs(2);

/// How often a slice reports lag while it runs. The end of a slice always reports.
const LAG_EVERY: Duration = Duration::from_secs(10);

/// The most the source may keep a slice waiting beyond its budget.
const READ_SLACK: Duration = Duration::from_secs(15);

/// A run older than this ends `succeeded` and a fresh one takes its place, so a trace, the
/// counters and the reject ratio span at most a day.
const ROTATE_AFTER: time::Duration = time::Duration::hours(24);

/// The lag a stream may have and still count as fresh when the source sets none.
const DEFAULT_MAX_LAG: i64 = 10_000;

pub(crate) struct KafkaConsume;

impl JobKind for KafkaConsume {
    const KIND: &'static str = SYNC_CONSUME_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        run_consume(ctx, &payload, &Probe::default(), KAFKA_SLICE_BUDGET).await
    }
}

/// Runs one slice. When an attempt fails and none is left, the run is marked `failed` with the
/// code of the failure, so it never stays `running` without a job.
pub(super) async fn run_consume(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    probe: &Probe,
    budget: Duration,
) -> Result<(), JobError> {
    let source_id =
        id_in(payload, "source_id").ok_or(JobError::new("the payload has no source_id"))?;
    let stream_id =
        id_in(payload, "stream_id").ok_or(JobError::new("the payload has no stream_id"))?;
    let run_id = id_in(payload, "run_id").ok_or(JobError::new("the payload has no run_id"))?;
    let generation = payload
        .get("generation")
        .and_then(Value::as_u64)
        .and_then(|generation| u32::try_from(generation).ok())
        .ok_or(JobError::new("the payload has no generation"))?;
    let workspace = ctx
        .workspace_id()
        .ok_or(JobError::new("the consumer job has no workspace"))?;
    let last = ctx.attempts() >= ctx.max_attempts();
    let pool = ctx
        .pool()
        .ok_or(JobError::new("the worker has no pool for consumers"))?
        .clone();
    let ids = Ids {
        workspace,
        source: source_id,
        stream: stream_id,
        run: run_id,
        lease: zirv_flow_store::SyncLease {
            job_id: ctx.job_id(),
            token: ctx.lease_token(),
        },
    };
    match slice(ctx, &pool, ids, generation, probe, budget).await {
        Ok(()) => Ok(()),
        Err(failure) => {
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

/// What a failed read means for the run.
enum ReadFailure {
    /// No attempt can succeed: the run ends with this code and the job is done.
    Final(&'static str),
    /// Another attempt may succeed; the run ends with this code if none is left.
    Retry(Failure),
}

/// The static run code of a source error, never its text.
fn read_failure(error: &SourceError) -> ReadFailure {
    match error {
        SourceError::Other(code) => match code.as_str() {
            "kafka_offset_out_of_range" => ReadFailure::Final("kafka_offset_out_of_range"),
            "kafka_auth_failed" => ReadFailure::Final("kafka_auth_failed"),
            "kafka_tls_failed" => ReadFailure::Final("kafka_tls_failed"),
            "kafka_topic_missing" => ReadFailure::Final("kafka_topic_missing"),
            "kafka_compression_unsupported" => ReadFailure::Final("kafka_compression_unsupported"),
            "kafka_connect_failed" => ReadFailure::Retry(coded("kafka_connect_failed")),
            "kafka_timeout" => ReadFailure::Retry(coded("kafka_connect_failed")),
            _ => ReadFailure::Retry(source_failure("the source could not be read")),
        },
        SourceError::Config { .. } => ReadFailure::Final("source_error"),
        _ => ReadFailure::Retry(source_failure("the source could not be read")),
    }
}

fn coded(code: &'static str) -> Failure {
    Failure {
        error: JobError::new("the source could not be read"),
        code,
    }
}

/// One message of the unit being gathered, with where it sat in the log.
struct Item {
    place: (i32, i64),
    what: What,
}

enum What {
    Record(Record),
    Refused(Refused),
}

/// `"<partition>/<offset>"`.
fn place_of_record(record: &Record) -> Option<(i32, i64)> {
    let (partition, offset) = record.occurrence.as_deref()?.split_once('/')?;
    Some((partition.parse().ok()?, offset.parse().ok()?))
}

/// `"partition <p> offset <o>"`.
fn place_of_refused(refused: &Refused) -> Option<(i32, i64)> {
    let rest = refused.location.strip_prefix("partition ")?;
    let (partition, offset) = rest.split_once(" offset ")?;
    Some((partition.parse().ok()?, offset.parse().ok()?))
}

/// The unit being gathered and what is committed.
struct Consumer<'a> {
    pass: Pass<'a>,
    items: Vec<Item>,
    /// The keys of the records in `items`: a second message with one of them starts a new unit,
    /// so no message is replaced by a later one of its key and each yields its own event.
    keys: BTreeSet<String>,
    first_at: Option<Instant>,
    observed_at: OffsetDateTime,
    /// The offsets committed, by partition.
    committed: BTreeMap<i32, i64>,
    /// The positions after everything the source has handed over.
    latest: Positions,
}

impl Consumer<'_> {
    /// Takes in a batch from the source: its messages in log order, and its positions.
    async fn absorb(&mut self, batch: Batch) -> Result<Unit, Failure> {
        let positions = Positions::from_checkpoint(&batch.checkpoint)
            .ok_or_else(|| Failure::job(JobError::new("the source sent a bad checkpoint")))?;
        if self.committed.is_empty() {
            // The first batch of a read carries the starting positions: the stored offsets, and
            // the earliest offset of a partition that has none.
            for (partition, position) in &positions.partitions {
                if let Ok(partition) = partition.parse::<i32>() {
                    self.committed.insert(partition, position.next);
                }
            }
        }
        let mut incoming = Vec::with_capacity(batch.records.len() + batch.refused.len());
        for record in batch.records {
            let place = place_of_record(&record)
                .ok_or_else(|| Failure::job(JobError::new("a message has no place")))?;
            incoming.push(Item {
                place,
                what: What::Record(record),
            });
        }
        for refused in batch.refused {
            let place = place_of_refused(&refused)
                .ok_or_else(|| Failure::job(JobError::new("a refusal has no place")))?;
            incoming.push(Item {
                place,
                what: What::Refused(refused),
            });
        }
        incoming.sort_by_key(|item| item.place);
        for item in incoming {
            if let What::Record(record) = &item.what
                && self.keys.contains(&record.key)
            {
                // The offsets can only cover what came before this message.
                if let Unit::Stop = self.flush(false).await? {
                    return Ok(Unit::Stop);
                }
            }
            if self.items.is_empty() {
                self.first_at = Some(Instant::now());
                self.observed_at = OffsetDateTime::now_utc();
            }
            if let What::Record(record) = &item.what {
                self.keys.insert(record.key.clone());
            }
            self.items.push(item);
            if self.items.len() >= COMMIT_MESSAGES {
                // A big batch commits in units of its own: only a prefix of it is in hand.
                if let Unit::Stop = self.flush(false).await? {
                    return Ok(Unit::Stop);
                }
            }
        }
        self.latest = positions;
        let due = self.items.len() >= COMMIT_MESSAGES
            || self
                .first_at
                .is_some_and(|first| first.elapsed() >= COMMIT_AFTER);
        if due {
            return self.flush(true).await;
        }
        Ok(Unit::Continue)
    }

    /// Commits what is gathered. `whole` says every message the source handed over is in the
    /// unit, so the offsets are its positions; otherwise only a prefix is, and each partition
    /// resumes after its last message in the unit.
    async fn flush(&mut self, whole: bool) -> Result<Unit, Failure> {
        if self.items.is_empty() {
            return if whole {
                self.store_moved().await
            } else {
                Ok(Unit::Continue)
            };
        }
        let mut offsets: BTreeMap<i32, PartitionOffset> = BTreeMap::new();
        if whole {
            for (partition, position) in &self.latest.partitions {
                if let Ok(partition) = partition.parse::<i32>() {
                    offsets.insert(
                        partition,
                        PartitionOffset {
                            partition,
                            next: position.next,
                            end: position.end,
                        },
                    );
                }
            }
        }
        for item in &self.items {
            let (partition, offset) = item.place;
            let entry = offsets.entry(partition).or_insert(PartitionOffset {
                partition,
                next: 0,
                end: self
                    .latest
                    .get(partition)
                    .and_then(|position: PartitionPosition| position.end),
            });
            if !whole {
                entry.next = entry.next.max(offset + 1);
            }
        }
        let offsets: Vec<PartitionOffset> = offsets.into_values().collect();
        let mut records = Vec::new();
        let mut refused = Vec::new();
        for item in std::mem::take(&mut self.items) {
            match item.what {
                What::Record(record) => records.push(record),
                What::Refused(item) => refused.push(item),
            }
        }
        self.keys.clear();
        self.first_at = None;
        let gathered = Gathered {
            records,
            refused,
            offsets: offsets.clone(),
            observed_at: self.observed_at,
        };
        let unit = self.pass.commit(gathered, &Checkpoint(Value::Null)).await?;
        if let Unit::Continue = unit {
            for offset in offsets {
                self.committed.insert(offset.partition, offset.next);
            }
        }
        Ok(unit)
    }

    /// Stores the positions that moved with no message to apply, such as a jump to the earliest
    /// offset when retention removed the one the read was at, so the stored lag does not stay
    /// at its old value until a message arrives.
    async fn store_moved(&mut self) -> Result<Unit, Failure> {
        let moved: Vec<PartitionOffset> = self
            .latest
            .partitions
            .iter()
            .filter_map(|(partition, position)| {
                let partition = partition.parse::<i32>().ok()?;
                (self.committed.get(&partition) != Some(&position.next)).then_some(
                    PartitionOffset {
                        partition,
                        next: position.next,
                        end: position.end,
                    },
                )
            })
            .collect();
        if moved.is_empty() {
            return Ok(Unit::Continue);
        }
        let ids = self.pass.ids;
        match commit_stream_offsets(
            self.pass.pool,
            ids.workspace,
            ids.stream,
            ids.run,
            ids.lease,
            &moved,
        )
        .await?
        {
            OffsetsStored::Stored => {
                for offset in moved {
                    self.committed.insert(offset.partition, offset.next);
                }
                Ok(Unit::Continue)
            }
            OffsetsStored::LeaseLost => Err(Failure::job(LEASE_LOST)),
            OffsetsStored::Cancelled => Ok(Unit::Stop),
        }
    }

    /// The committed offsets with the watermarks last seen: what lag is measured from.
    fn lag_offsets(&self) -> Vec<PartitionOffset> {
        self.committed
            .iter()
            .map(|(partition, next)| PartitionOffset {
                partition: *partition,
                next: *next,
                end: self
                    .latest
                    .get(*partition)
                    .and_then(|position| position.end),
            })
            .collect()
    }
}

/// Reports lag and freshness. `Stop` when the run is over.
async fn report_lag(
    pool: &PgPool,
    ids: Ids,
    consumer: &Consumer<'_>,
    max_lag: i64,
) -> Result<Unit, Failure> {
    let offsets = consumer.lag_offsets();
    if offsets.is_empty() {
        return Ok(Unit::Continue);
    }
    match report_stream_lag(
        pool,
        ids.workspace,
        ids.stream,
        ids.run,
        ids.lease,
        &offsets,
        max_lag,
    )
    .await?
    {
        LagReport::Reported { .. } => Ok(Unit::Continue),
        LagReport::LeaseLost => Err(Failure::job(LEASE_LOST)),
        LagReport::Cancelled => Ok(Unit::Stop),
    }
}

async fn slice(
    ctx: &mut JobContext<'_>,
    pool: &PgPool,
    ids: Ids,
    generation: u32,
    probe: &Probe,
    budget: Duration,
) -> Result<(), Failure> {
    // Cancelled, or finished by an earlier attempt: nothing to do, and no successor.
    let Some(run) = start_stream_run(pool, ids.workspace, ids.run, ctx.attempts() > 1).await?
    else {
        return Ok(());
    };
    let prepared = match prepare(ctx, pool, ids, &run).await? {
        Prep::Ready(prepared) => prepared,
        Prep::Ended => return Ok(()),
    };
    let Prepared {
        source,
        stream,
        mode,
        redaction_key,
        driver,
        context,
        config,
        ..
    } = prepared.as_ref();
    if *mode != SyncMode::Stream {
        return end_failed(pool, ids, "stream_settings_invalid").await;
    }
    // A run that has lived a day ends and a fresh one takes over, in one transaction.
    if run
        .started_at
        .is_some_and(|started| OffsetDateTime::now_utc() - started > ROTATE_AFTER)
    {
        return match rotate_stream_run(pool, ids.workspace, ids.source, ids.run, ids.lease).await? {
            SyncRotated::Rotated(next) => {
                info!(run_id = %ids.run, next_run_id = %next.id, "consumer run rotated");
                record_sync_run(RunOutcome::Succeeded, 0.0);
                Ok(())
            }
            SyncRotated::Over => Ok(()),
            SyncRotated::LeaseLost => Err(Failure::job(LEASE_LOST)),
        };
    }

    let mut start = Positions::default();
    for offset in load_offsets(pool, ids.workspace, ids.stream).await? {
        start.set(
            offset.partition,
            PartitionPosition {
                next: offset.next,
                end: offset.end,
            },
        );
    }
    let max_lag = source
        .config
        .get("max_lag")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_LAG)
        .max(0);
    let plan = ReadPlan {
        budget: Some(budget),
        stop_when_caught_up: false,
    };
    let planner = prepared.planner();
    let pass = Pass {
        pool,
        ids,
        stream,
        planner: &planner,
        job: (ctx.job_id(), ctx.lease_token(), ctx.attempts()),
        started: run.started_at,
        probe,
        committed: 0,
        reconcile: false,
        redaction_key: redaction_key.clone(),
    };
    let mut consumer = Consumer {
        pass,
        items: Vec::new(),
        keys: BTreeSet::new(),
        first_at: None,
        observed_at: OffsetDateTime::now_utc(),
        committed: BTreeMap::new(),
        latest: Positions::default(),
    };

    // The credential is opened right before the read and dropped with it.
    // A consumer opens it every slice, so the use is one row an hour.
    let credential = open_credential(
        ctx,
        pool,
        ids.workspace,
        source.credential_id,
        Some(Duration::from_secs(3600)),
    )
    .await?;
    let mut batches = driver.read(
        context,
        &source.config,
        &credential,
        config,
        Some(plan.into_checkpoint(&start)),
    );
    let started = Instant::now();
    let mut last_report = Instant::now();
    loop {
        let wait = budget.saturating_sub(started.elapsed()) + READ_SLACK;
        let next = tokio::time::timeout(wait, batches.next())
            .await
            .map_err(|_| source_failure("the source did not answer in time"))?;
        match next {
            Some(Ok(batch)) => {
                if let Unit::Stop = consumer.absorb(batch).await? {
                    return Ok(());
                }
                if last_report.elapsed() >= LAG_EVERY {
                    last_report = Instant::now();
                    if let Unit::Stop = report_lag(pool, ids, &consumer, max_lag).await? {
                        return Ok(());
                    }
                }
            }
            Some(Err(error)) => {
                warn!(run_id = %ids.run, "a consumer read failed");
                return match read_failure(&error) {
                    ReadFailure::Final(code) => end_failed(pool, ids, code).await,
                    ReadFailure::Retry(failure) => Err(failure),
                };
            }
            None => break,
        }
    }
    if let Unit::Stop = consumer.flush(true).await? {
        return Ok(());
    }
    if let Unit::Stop = report_lag(pool, ids, &consumer, max_lag).await? {
        return Ok(());
    }
    let mut next = NewJob::new(SYNC_CONSUME_JOB_KIND, run_trace(&run));
    next.workspace_id = Some(ids.workspace);
    next.payload = json!({
        "source_id": ids.source,
        "stream_id": ids.stream,
        "run_id": ids.run,
        "generation": generation + 1,
    });
    next.unique_key = Some(sync_consume_unique_key(ids.run, generation + 1));
    ctx.enqueue(&next).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(occurrence: Option<&str>) -> Record {
        Record {
            key: "k".into(),
            data: Value::Null,
            deleted: true,
            occurrence: occurrence.map(str::to_owned),
        }
    }

    #[test]
    fn the_place_of_a_message_is_read_from_its_occurrence_or_its_location() {
        assert_eq!(place_of_record(&record(Some("3/1042"))), Some((3, 1042)));
        assert_eq!(place_of_record(&record(Some("x/1"))), None);
        assert_eq!(place_of_record(&record(None)), None);
        let refused = Refused {
            location: "partition 7 offset 12".into(),
            code: "kafka_value_not_json",
        };
        assert_eq!(place_of_refused(&refused), Some((7, 12)));
        let bad = Refused {
            location: "somewhere".into(),
            code: "kafka_value_not_json",
        };
        assert_eq!(place_of_refused(&bad), None);
    }

    #[test]
    fn a_source_error_becomes_a_static_run_code_and_says_whether_to_retry() {
        let other = |code: &str| SourceError::Other(code.to_owned());
        for code in [
            "kafka_offset_out_of_range",
            "kafka_auth_failed",
            "kafka_tls_failed",
            "kafka_topic_missing",
            "kafka_compression_unsupported",
        ] {
            assert!(
                matches!(read_failure(&other(code)), ReadFailure::Final(found) if found == code),
                "{code}"
            );
        }
        for code in ["kafka_connect_failed", "kafka_timeout"] {
            assert!(
                matches!(read_failure(&other(code)), ReadFailure::Retry(failure) if failure.code == "kafka_connect_failed"),
                "{code}"
            );
        }
        let config = SourceError::Config {
            field: "topics".into(),
            reason: "is bad".into(),
        };
        assert!(matches!(
            read_failure(&config),
            ReadFailure::Final("source_error")
        ));
        assert!(matches!(
            read_failure(&SourceError::Unreachable),
            ReadFailure::Retry(failure) if failure.code == "source_error"
        ));
    }

    #[test]
    fn the_slice_is_shorter_than_the_usual_shutdown_wait() {
        const { assert!(KAFKA_SLICE_BUDGET.as_secs() <= 60) };
        const { assert!(COMMIT_MESSAGES == 500) };
        assert_eq!(COMMIT_AFTER, Duration::from_secs(2));
    }
}

/// The consumer against a real broker and PostgreSQL. `#[ignore]`d: `scripts/kafka-fixture.sh`
/// starts the broker and prints `ZIRV_KAFKA_BROKER`; `DATABASE_URL` names the database.
#[cfg(test)]
mod broker_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rskafka::client::ClientBuilder;
    use rskafka::client::partition::{Compression, UnknownTopicHandling};
    use rskafka::record::Record as KafkaRecord;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};
    use zirv_flow_store::{
        ClaimedJob, DiscoveredStream, NewSource, NewTraitDefinition, RunKind, RunTrigger,
        StreamSettings, begin, cancel_run, claim_job, commit, configure_stream, create_source,
        create_trait_definition, create_workspace, find_run, queue_run, set_stream_mapping,
        stream_freshness, upsert_discovered_streams,
    };

    use super::*;
    use crate::context::IngestSettings;

    fn broker() -> String {
        std::env::var("ZIRV_KAFKA_BROKER")
            .unwrap_or_else(|_| panic!("ZIRV_KAFKA_BROKER is not set; run kafka-fixture.sh"))
    }

    fn unique(prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// A producer connection straight to the broker, which advertises `localhost`.
    async fn client() -> rskafka::client::Client {
        ClientBuilder::new(vec![broker()]).build().await.unwrap()
    }

    async fn create_topic(topic: &str, partitions: i32) {
        let client = client().await;
        client
            .controller_client()
            .unwrap()
            .create_topic(topic, partitions, 1, 10_000)
            .await
            .unwrap();
        for _ in 0..50 {
            let topics = client.list_topics().await.unwrap();
            if topics
                .iter()
                .any(|t| t.name == topic && t.partitions.len() == partitions as usize)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the topic did not appear");
    }

    fn message(key: Option<&str>, value: Option<Vec<u8>>) -> KafkaRecord {
        KafkaRecord {
            key: key.map(|key| key.as_bytes().to_vec()),
            value,
            headers: BTreeMap::new(),
            timestamp: chrono_now(),
        }
    }

    fn chrono_now() -> rskafka::chrono::DateTime<rskafka::chrono::Utc> {
        rskafka::chrono::Utc::now()
    }

    fn user(i: usize) -> Option<Vec<u8>> {
        Some(
            json!({"email": format!("u{i}@example.com"), "name": format!("User {i}"), "member": "yes"})
                .to_string()
                .into_bytes(),
        )
    }

    async fn produce(topic: &str, partition: i32, records: Vec<KafkaRecord>) {
        client()
            .await
            .partition_client(topic, partition, UnknownTopicHandling::Retry)
            .await
            .unwrap()
            .produce(records, Compression::NoCompression)
            .await
            .unwrap();
    }

    /// `upserts` users spread over three partitions (user `i` on partition `i % 3`), the
    /// tombstones of users `0..tombstones` after them on the same partitions, and four
    /// malformed messages. Returns how many messages were produced.
    async fn load(topic: &str, upserts: usize, tombstones: usize) -> usize {
        create_topic(topic, 3).await;
        let mut by_partition: Vec<Vec<KafkaRecord>> = vec![Vec::new(), Vec::new(), Vec::new()];
        for i in 0..upserts {
            by_partition[i % 3].push(message(Some(&format!("u{i}")), user(i)));
        }
        for i in 0..tombstones {
            by_partition[i % 3].push(message(Some(&format!("u{i}")), None));
        }
        by_partition[0].push(message(None, user(0)));
        by_partition[1].push(message(Some("bad1"), Some(b"{not json".to_vec())));
        by_partition[2].push(message(Some("bad2"), Some(b"[1,2]".to_vec())));
        let total = by_partition.iter().map(Vec::len).sum::<usize>() + 1;
        for (partition, records) in by_partition.into_iter().enumerate() {
            for chunk in records.chunks(200) {
                produce(topic, partition as i32, chunk.to_vec()).await;
            }
        }
        // Over the limit once decompressed, yet small enough on the wire for the broker.
        client()
            .await
            .partition_client(topic, 2, UnknownTopicHandling::Retry)
            .await
            .unwrap()
            .produce(
                vec![message(Some("bad3"), Some(vec![b' '; (1 << 20) + 1]))],
                Compression::Gzip,
            )
            .await
            .unwrap();
        total
    }

    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        source: Uuid,
        stream: Uuid,
    }

    fn policy(allow_loopback: bool) -> Arc<EgressPolicy> {
        let entries: Vec<String> = if allow_loopback {
            vec!["127.0.0.0/8".to_owned(), "::1/128".to_owned()]
        } else {
            Vec::new()
        };
        Arc::new(EgressPolicy::parse(&entries).unwrap())
    }

    fn descriptor(topic: &str) -> Value {
        json!({
            "name": topic,
            "fields": [
                {"name": "_key", "field_type": "string"},
                {"name": "_timestamp", "field_type": "timestamp"},
                {"name": "email", "field_type": "string"},
                {"name": "name", "field_type": "string"},
                {"name": "member", "field_type": "string"},
            ],
            "sync_modes": ["stream"],
            "default_cursor_field": null,
            "default_primary_key": ["_key"],
        })
    }

    fn mapping() -> Value {
        json!({
            "columns": [
                {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                {"column": "name", "target": {"type": "trait", "key": "display_name"}},
            ]
        })
    }

    async fn setup_with(
        pool: &PgPool,
        slug: &str,
        topic: &str,
        config: Value,
        mapping: Value,
        primary_key: &[String],
    ) -> Env {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = create_workspace(
            pool,
            &WorkspaceName::parse(slug).unwrap(),
            &Slug::parse(slug).unwrap(),
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
        let mut full = json!({"brokers": broker(), "topics": topic, "security": "plaintext"});
        full.as_object_mut()
            .unwrap()
            .extend(config.as_object().unwrap().clone());
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "kafka",
                name: "events",
                config: &full,
                credential_id: None,
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let described = descriptor(topic);
        let mut tx = begin(pool).await.unwrap();
        let streams = upsert_discovered_streams(
            &mut tx,
            ws,
            source.id,
            &[DiscoveredStream {
                name: topic,
                sync_mode: "stream",
                cursor_field: None,
                primary_key,
                descriptor: &described,
                suggested_mapping: None,
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
            topic,
            &StreamSettings {
                enabled: true,
                sync_mode: "stream",
                cursor_field: None,
                primary_key,
                staleness_threshold_seconds: None,
                reconcile_schedule: None,
                sweep_limit: 0.5,
            },
        )
        .await
        .unwrap();
        set_stream_mapping(pool, ws, source.id, topic, &mapping)
            .await
            .unwrap();
        Env {
            pool: pool.clone(),
            ws,
            source: source.id,
            stream: streams[0].id,
        }
    }

    async fn setup(pool: &PgPool, slug: &str, topic: &str) -> Env {
        setup_with(
            pool,
            slug,
            topic,
            json!({}),
            mapping(),
            &["_key".to_owned()],
        )
        .await
    }

    impl Env {
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
                &["kafka.consume"],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
            .unwrap()
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

        async fn count(&self, sql: &'static str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }

        async fn run(&self, id: Uuid) -> zirv_flow_store::SyncRun {
            find_run(&self.pool, self.ws, self.source, id)
                .await
                .unwrap()
                .unwrap()
        }

        async fn offsets(&self) -> Vec<(i32, i64, Option<i64>)> {
            sqlx::query_as(
                "SELECT partition, next_offset, high_watermark FROM stream_offsets \
                 WHERE stream_id = $1 ORDER BY partition",
            )
            .bind(self.stream)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// What the run left behind, comparable between two runs of the same data.
        async fn shape(&self) -> (i64, i64, i64, Vec<String>, i64) {
            let scoped = |sql: &'static str| {
                let pool = self.pool.clone();
                let ws = self.ws.as_uuid();
                async move {
                    sqlx::query_scalar::<_, i64>(sql)
                        .bind(ws)
                        .fetch_one(&pool)
                        .await
                        .unwrap()
                }
            };
            let live = scoped(
                "SELECT count(*) FROM source_records WHERE workspace_id = $1 AND deleted_at IS NULL",
            )
            .await;
            let removed = scoped(
                "SELECT count(*) FROM source_records \
                 WHERE workspace_id = $1 AND deleted_at IS NOT NULL",
            )
            .await;
            let rejects =
                scoped("SELECT count(*) FROM ingest_rejects WHERE workspace_id = $1").await;
            let outcomes: Vec<String> = sqlx::query_scalar(
                "SELECT outcome || ':' || count(*)::text FROM decision_records \
                 WHERE workspace_id = $1 AND action = 'sync.record' GROUP BY outcome ORDER BY outcome",
            )
            .bind(self.ws.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap();
            let people = scoped("SELECT count(*) FROM people WHERE workspace_id = $1").await;
            (live, rejects, people, outcomes, removed)
        }
    }

    async fn attempt_with(
        env: &Env,
        job: &ClaimedJob,
        probe: &Probe,
        budget: Duration,
        allow_loopback: bool,
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
                .with_outbound(None, policy(allow_loopback), Arc::default());
            run_consume(&mut ctx, &job.payload, probe, budget).await
        };
        if result.is_ok() {
            commit(tx).await.unwrap();
        }
        result
    }

    const SLICE: Duration = Duration::from_millis(2500);

    async fn attempt(env: &Env, job: &ClaimedJob) -> Result<(), JobError> {
        attempt_with(env, job, &Probe::default(), SLICE, true).await
    }

    #[test]
    fn the_kind_name_is_kafka_consume() {
        assert_eq!(KafkaConsume::KIND, "kafka.consume");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_topic_of_upserts_tombstones_and_malformed_messages_applies_each_once_and_hands_over(
        pool: PgPool,
    ) {
        let topic = unique("e2e");
        let total = load(&topic, 30, 6).await;
        let env = setup(&pool, "one", &topic).await;
        let (run, job) = env.queue().await;

        attempt(&env, &job).await.unwrap();

        let found = env.run(run.id).await;
        assert_eq!(
            found.state, "running",
            "a consumer run goes on across slices"
        );
        assert_eq!(found.attempt, 1);
        // 30 users, 6 of them removed again; every malformed message is a reject.
        let live: i64 = env
            .count("SELECT count(*) FROM source_records WHERE deleted_at IS NULL")
            .await;
        assert_eq!(live, 24);
        assert_eq!(found.records_deleted, 6);
        assert_eq!(found.records_rejected, 4);
        assert_eq!(found.records_read as usize, total);
        let codes: Vec<String> =
            sqlx::query_scalar("SELECT reasons->0->>'reason' FROM ingest_rejects ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            codes,
            [
                "kafka_key_missing",
                "kafka_message_too_large",
                "kafka_value_not_json",
                "kafka_value_not_object"
            ]
        );
        let places: Vec<String> =
            sqlx::query_scalar("SELECT reasons->0->>'property' FROM ingest_rejects ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            places.iter().all(|place| place.starts_with("partition ")),
            "{places:?}"
        );
        // The offsets are the consumed counts: 10 + 2 (+1 keyless), ...
        let offsets = env.offsets().await;
        assert_eq!(offsets.len(), 3);
        assert_eq!(offsets.iter().map(|o| o.1).sum::<i64>() as usize, total);
        assert!(
            offsets.iter().all(|o| o.2 == Some(o.1)),
            "caught up: {offsets:?}"
        );
        // The successor is queued under the next generation's key; the stream is fresh with no
        // lag.
        let keys: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT unique_key FROM jobs WHERE kind = 'kafka.consume' AND id <> $1",
        )
        .bind(job.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(keys, [Some(format!("kafka:consume:{}:1", run.id))]);
        let now = zirv_flow_store::database_now(&pool).await.unwrap();
        let fresh = stream_freshness(&pool, Some(env.ws), None, now)
            .await
            .unwrap();
        assert_eq!(fresh[0].lag, Some(0));
        assert!(fresh[0].last_success_at.is_some());
        // Decisions and the trace are the pull path's, under the run's trace.
        let traces: Vec<Uuid> =
            sqlx::query_scalar("SELECT DISTINCT trace_id FROM decision_records")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(traces, [run.trace_id]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_slice_killed_between_commits_and_resumed_equals_an_uninterrupted_run(pool: PgPool) {
        let (interrupted_topic, control_topic) = (unique("kill"), unique("control"));
        let total = load(&interrupted_topic, 1100, 120).await;
        assert_eq!(load(&control_topic, 1100, 120).await, total);
        let interrupted = setup(&pool, "one", &interrupted_topic).await;
        let control = setup(&pool, "two", &control_topic).await;

        // The control reads everything in one go.
        let (_, control_job) = control.queue().await;
        attempt_with(
            &control,
            &control_job,
            &Probe::default(),
            Duration::from_secs(6),
            true,
        )
        .await
        .unwrap();

        // The control's successor must not be claimed in place of the interrupted run's job.
        sqlx::query("DELETE FROM jobs WHERE state = 'ready'")
            .execute(&pool)
            .await
            .unwrap();

        // The first attempt dies after its first unit, the second resumes from the offsets the
        // first committed.
        let (run, job) = interrupted.queue().await;
        let died = attempt_with(
            &interrupted,
            &job,
            &Probe::stopping_after(Some(1)),
            Duration::from_secs(6),
            true,
        )
        .await;
        assert!(died.is_err());
        let partial: i64 = interrupted.offsets().await.iter().map(|o| o.1).sum();
        assert!(
            partial > 0 && (partial as usize) < total,
            "one unit is committed: {partial} of {total}"
        );
        let again = interrupted.reclaim(&job).await;
        assert_eq!(again.attempts, 2);
        attempt_with(
            &interrupted,
            &again,
            &Probe::default(),
            Duration::from_secs(6),
            true,
        )
        .await
        .unwrap();

        // The same records, rejects, decisions and offsets as the uninterrupted run.
        assert_eq!(interrupted.shape().await, control.shape().await);
        let (live, rejects, _, _, removed) = interrupted.shape().await;
        assert_eq!((live, removed, rejects), (1100 - 120, 120, 4));
        let consumed: i64 = interrupted.offsets().await.iter().map(|o| o.1).sum();
        assert_eq!(
            consumed as usize, total,
            "the offsets equal the consumed counts"
        );
        assert_eq!(interrupted.offsets().await, control.offsets().await);
        let found = interrupted.run(run.id).await;
        assert_eq!(
            found.attempt, 2,
            "the lost attempt is visible as an attempt"
        );
        assert_eq!(
            found.records_read as usize, total,
            "no message counted twice"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_cancelled_run_gets_no_slice_and_no_successor(pool: PgPool) {
        let topic = unique("cancel");
        load(&topic, 5, 0).await;
        let env = setup(&pool, "one", &topic).await;
        let (run, job) = env.queue().await;
        cancel_run(
            &mut pool.acquire().await.unwrap(),
            env.ws,
            env.source,
            run.id,
        )
        .await
        .unwrap();

        attempt(&env, &job).await.unwrap();

        assert!(env.offsets().await.is_empty());
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'kafka.consume'")
                .await,
            1,
            "only the job that just ran"
        );
        assert_eq!(env.run(run.id).await.state, "cancelled");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_missing_topic_a_recreated_topic_and_a_bad_setting_end_the_run_with_their_code_and_no_successor(
        pool: PgPool,
    ) {
        // A topic that does not exist.
        let missing = setup(&pool, "one", &unique("missing")).await;
        let (run, job) = missing.queue().await;
        attempt(&missing, &job).await.unwrap();
        let ended = missing.run(run.id).await;
        assert_eq!(
            (ended.state.as_str(), ended.error_code.as_deref()),
            ("failed", Some("kafka_topic_missing"))
        );
        assert_eq!(
            missing
                .count("SELECT count(*) FROM jobs WHERE kind = 'kafka.consume'")
                .await,
            1
        );

        // A stored offset past the end of the log: the topic was recreated.
        let topic = unique("recreated");
        load(&topic, 3, 0).await;
        let recreated = setup(&pool, "two", &topic).await;
        sqlx::query(
            "INSERT INTO stream_offsets (workspace_id, stream_id, partition, next_offset) \
             VALUES ($1, $2, 0, 9999)",
        )
        .bind(recreated.ws.as_uuid())
        .bind(recreated.stream)
        .execute(&pool)
        .await
        .unwrap();
        let (run, job) = recreated.queue().await;
        attempt(&recreated, &job).await.unwrap();
        let ended = recreated.run(run.id).await;
        assert_eq!(
            (ended.state.as_str(), ended.error_code.as_deref()),
            ("failed", Some("kafka_offset_out_of_range"))
        );
        assert_eq!(
            recreated
                .count("SELECT count(*) FROM stream_offsets WHERE next_offset <> 9999")
                .await,
            0
        );

        // A topics setting that does not name the stream's topic.
        let topic = unique("wrongtopic");
        load(&topic, 1, 0).await;
        let misconfigured = setup_with(
            &pool,
            "three",
            &topic,
            json!({"topics": "somewhere-else"}),
            mapping(),
            &["_key".to_owned()],
        )
        .await;
        let (run, job) = misconfigured.queue().await;
        attempt(&misconfigured, &job).await.unwrap();
        assert_eq!(
            misconfigured.run(run.id).await.error_code.as_deref(),
            Some("source_error")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn an_unusable_mapping_ends_the_run_before_any_read(pool: PgPool) {
        let topic = unique("badmapping");
        load(&topic, 3, 0).await;
        let broken = json!({"columns": [
            {"column": "no_such_field", "target": {"type": "identifier", "kind": "email"}}
        ]});
        let env = setup_with(
            &pool,
            "one",
            &topic,
            json!({}),
            broken,
            &["_key".to_owned()],
        )
        .await;
        let (run, job) = env.queue().await;

        attempt(&env, &job).await.unwrap();

        assert_eq!(
            env.run(run.id).await.error_code.as_deref(),
            Some("mapping_invalid")
        );
        assert!(env.offsets().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn an_event_mapping_needs_a_key_and_a_redaction_key_or_the_run_ends(pool: PgPool) {
        let with_event = json!({
            "columns": [{"column": "email", "target": {"type": "identifier", "kind": "email"}}],
            "event": {"name": "seen"}
        });
        let topic = unique("events");
        load(&topic, 2, 0).await;

        // No primary key: the run ends `stream_not_keyed`.
        let unkeyed = setup_with(&pool, "one", &topic, json!({}), with_event.clone(), &[]).await;
        let (run, job) = unkeyed.queue().await;
        attempt(&unkeyed, &job).await.unwrap();
        assert_eq!(
            unkeyed.run(run.id).await.error_code.as_deref(),
            Some("stream_not_keyed")
        );

        // A key but no redaction key on the worker: `event_key_unavailable`.
        let keyed = setup_with(
            &pool,
            "two",
            &topic,
            json!({}),
            with_event,
            &["_key".to_owned()],
        )
        .await;
        let (run, job) = keyed.queue().await;
        attempt(&keyed, &job).await.unwrap();
        assert_eq!(
            keyed.run(run.id).await.error_code.as_deref(),
            Some("event_key_unavailable")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_mapping_that_names_a_missing_list_ends_the_run_and_writes_no_offset(pool: PgPool) {
        let topic = unique("nolist");
        load(&topic, 3, 0).await;
        let listed = json!({"columns": [
            {"column": "email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "member", "target": {"type": "list", "list_id": Uuid::now_v7()}}
        ]});
        let env = setup_with(
            &pool,
            "one",
            &topic,
            json!({}),
            listed,
            &["_key".to_owned()],
        )
        .await;
        let (run, job) = env.queue().await;

        attempt(&env, &job).await.unwrap();

        assert_eq!(
            env.run(run.id).await.error_code.as_deref(),
            Some("list_missing")
        );
        assert!(env.offsets().await.is_empty());
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_run_over_the_reject_threshold_stops_and_leaves_no_successor(pool: PgPool) {
        let topic = unique("threshold");
        create_topic(&topic, 1).await;
        let bad: Vec<KafkaRecord> = (0..150)
            .map(|i| message(Some(&format!("k{i}")), Some(b"nope".to_vec())))
            .collect();
        produce(&topic, 0, bad).await;
        let env = setup(&pool, "one", &topic).await;
        let (run, job) = env.queue().await;

        attempt(&env, &job).await.unwrap();

        let ended = env.run(run.id).await;
        assert_eq!(
            (ended.state.as_str(), ended.error_code.as_deref()),
            ("failed", Some("reject_threshold"))
        );
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'kafka.consume'")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_broker_the_egress_policy_refuses_fails_the_run_with_source_error_and_no_connection(
        pool: PgPool,
    ) {
        let topic = unique("egress");
        load(&topic, 2, 0).await;
        let env = setup(&pool, "one", &topic).await;
        let (run, job) = env.queue().await;
        // Only one attempt is left, so the failure ends the run.
        sqlx::query("UPDATE jobs SET max_attempts = 1")
            .execute(&pool)
            .await
            .unwrap();
        let job = env.reclaim(&job).await;
        assert_eq!(job.max_attempts, 1);

        let result = attempt_with(&env, &job, &Probe::default(), SLICE, false).await;

        assert!(result.is_err());
        let ended = env.run(run.id).await;
        assert_eq!(
            (ended.state.as_str(), ended.error_code.as_deref()),
            ("failed", Some("source_error"))
        );
        assert!(env.offsets().await.is_empty());
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_run_older_than_a_day_rotates_to_a_fresh_run_in_one_step(pool: PgPool) {
        let topic = unique("rotate");
        load(&topic, 3, 0).await;
        let env = setup(&pool, "one", &topic).await;
        let (run, job) = env.queue().await;
        zirv_flow_store::start_stream_run(&pool, env.ws, run.id, false)
            .await
            .unwrap();
        sqlx::query("UPDATE sync_runs SET started_at = now() - interval '25 hours'")
            .execute(&pool)
            .await
            .unwrap();

        attempt(&env, &job).await.unwrap();

        assert_eq!(env.run(run.id).await.state, "succeeded");
        let next: Vec<(Uuid, String, String)> =
            sqlx::query_as("SELECT id, state, trigger FROM sync_runs WHERE id <> $1")
                .bind(run.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(
            (next[0].1.as_str(), next[0].2.as_str()),
            ("queued", "schedule")
        );
        assert!(
            env.offsets().await.is_empty(),
            "the rotating slice reads nothing"
        );
        let keys: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT unique_key FROM jobs WHERE kind = 'kafka.consume' AND id <> $1",
        )
        .bind(job.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(keys, [Some(format!("kafka:consume:{}:0", next[0].0))]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_second_slice_continues_where_the_first_stopped_and_reports_lag_above_the_bound(
        pool: PgPool,
    ) {
        let topic = unique("lag");
        create_topic(&topic, 1).await;
        produce(
            &topic,
            0,
            (0..4)
                .map(|i| message(Some(&format!("u{i}")), user(i)))
                .collect(),
        )
        .await;
        let env = setup_with(
            &pool,
            "one",
            &topic,
            json!({"max_lag": 0}),
            mapping(),
            &["_key".to_owned()],
        )
        .await;
        let (run, job) = env.queue().await;
        attempt(&env, &job).await.unwrap();
        assert_eq!(env.offsets().await, [(0, 4, Some(4))]);

        // New messages arrive while no slice runs; the next slice picks the successor up.
        produce(
            &topic,
            0,
            (4..9)
                .map(|i| message(Some(&format!("u{i}")), user(i)))
                .collect(),
        )
        .await;
        let next = env.claim().await;
        assert_eq!(next.unique_key, Some(format!("kafka:consume:{}:1", run.id)));
        attempt(&env, &next).await.unwrap();

        assert_eq!(env.offsets().await, [(0, 9, Some(9))]);
        assert_eq!(env.count("SELECT count(*) FROM source_records").await, 9);
        let found = env.run(run.id).await;
        assert_eq!(found.attempt, 1, "a new slice is not a lost attempt");
        assert_eq!(found.records_read, 9);
    }
}
