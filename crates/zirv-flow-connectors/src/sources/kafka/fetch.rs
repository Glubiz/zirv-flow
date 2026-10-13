//! Reading the partitions of one topic: where each starts, how a read advances past offsets that
//! hold no record (spec D10), and the round-robin loop that turns fetches into batches.
//!
//! The fetch itself sits behind [`PartitionFetch`], so the logic is tested with a scripted fake.

use std::any::Any;
use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::join_all;
use futures_util::stream::{self, BoxStream};
use rskafka::record::RecordAndOffset;
use tokio::time::{Instant, sleep};

use super::super::error::SourceError;
use super::super::types::{Batch, Refused};
use super::error::{FetchError, OFFSET_OUT_OF_RANGE, code};
use super::message::{Converted, convert};
use super::positions::{PartitionPosition, Positions, ReadPlan};

/// How many partitions are fetched at once.
pub(super) const IN_FLIGHT: usize = 4;

/// How long a pass that returned nothing waits before the next.
pub(super) const IDLE_SLEEP: Duration = Duration::from_secs(1);

/// Consecutive empty fetches below the high watermark before the reader probes past the gap.
const GAP_STREAK: u8 = 2;

/// How many offsets after `next` one pass probes for the record that ends a gap.
const PROBE_SPAN: i64 = 16;

/// The calls a read makes on one partition.
pub(super) trait PartitionFetch: Send + Sync {
    /// The earliest offset still in the log.
    fn earliest(&self) -> impl Future<Output = Result<i64, FetchError>> + Send;

    /// The offset after the last record.
    fn latest(&self) -> impl Future<Output = Result<i64, FetchError>> + Send;

    /// The records from `offset` on and the high watermark.
    fn fetch(
        &self,
        offset: i64,
    ) -> impl Future<Output = Result<(Vec<RecordAndOffset>, i64), FetchError>> + Send;
}

/// One partition being read.
pub(super) struct Partition<F> {
    pub(super) id: i32,
    fetcher: F,
    next: i64,
    end: i64,
    /// Empty fetches in a row while the log reaches past `next`.
    streak: u8,
    /// Where the next probe starts when the last one stopped short of the gap's end; `None`
    /// starts it right after `next`.
    probe_from: Option<i64>,
    /// The last pass found nothing more to deliver.
    settled: bool,
}

impl<F> Partition<F> {
    pub(super) fn position(&self) -> PartitionPosition {
        PartitionPosition {
            next: self.next,
            end: Some(self.end),
        }
    }

    /// Nothing more to deliver for now.
    pub(super) fn settled(&self) -> bool {
        self.settled
    }
}

impl<F: PartitionFetch> Partition<F> {
    /// Places the partition: at its stored offset, at its earliest when it has none, at its
    /// earliest when retention removed the stored one (counted), and refused when the stored one
    /// is past the end (the topic was recreated).
    pub(super) async fn open(
        id: i32,
        fetcher: F,
        stored: Option<i64>,
    ) -> Result<Self, SourceError> {
        let earliest = fetcher.earliest().await?;
        let latest = fetcher.latest().await?;
        let next = match stored {
            None => earliest,
            Some(offset) if offset < earliest => {
                metrics::counter!("zirv_flow_kafka_offset_resets_total").increment(1);
                earliest
            }
            Some(offset) if offset > latest => return Err(code(OFFSET_OUT_OF_RANGE)),
            Some(offset) => offset,
        };
        Ok(Self {
            id,
            fetcher,
            next,
            end: latest,
            streak: 0,
            probe_from: None,
            settled: false,
        })
    }

    /// One fetch, with the gap logic: after two empty fetches while the log reaches past `next`
    /// (a control batch, a compacted-away batch or an open transaction), probes the offsets after
    /// `next`; a record at one means everything below it cannot be delivered, so the partition
    /// jumps to it.
    pub(super) async fn pass(&mut self) -> Result<Vec<RecordAndOffset>, SourceError> {
        let fetched = self.fetcher.fetch(self.next).await;
        metrics::counter!(
            "zirv_flow_kafka_fetches_total",
            "outcome" => if fetched.is_ok() { "ok" } else { "error" }
        )
        .increment(1);
        let (mut records, high) = match fetched {
            Ok(fetched) => fetched,
            Err(FetchError::OffsetOutOfRange) => {
                self.recover().await?;
                self.settled = false;
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        };
        self.end = high;
        records.retain(|message| message.offset >= self.next);
        self.settled = false;
        if records.is_empty() {
            if high > self.next {
                self.streak = self.streak.saturating_add(1);
                if self.streak >= GAP_STREAK {
                    match self.probe(high).await? {
                        Some(probe) => {
                            self.next = probe[0].offset;
                            self.streak = 0;
                            records = probe;
                        }
                        None => self.settled = true,
                    }
                }
            } else {
                self.streak = 0;
                self.settled = true;
            }
        } else {
            self.streak = 0;
        }
        if let Some(last) = records.last() {
            self.next = last.offset + 1;
            self.probe_from = None;
        }
        Ok(records)
    }

    /// Fetches from the probe cursor (`next + 1` when there is none), at most [`PROBE_SPAN`]
    /// offsets below `high`, and returns the records of the first fetch that holds one above
    /// `next`. A gap longer than one pass is crossed over several: the cursor keeps where this
    /// probe stopped, and wraps to `next + 1` once it reaches `high`. An empty probe proves
    /// nothing (an open transaction fetches empty too), so the partition never moves past one.
    async fn probe(&mut self, high: i64) -> Result<Option<Vec<RecordAndOffset>>, SourceError> {
        let first = self.probe_from.unwrap_or(0).max(self.next + 1);
        if first >= high {
            self.probe_from = None;
            return Ok(None);
        }
        let last = high.min(first.saturating_add(PROBE_SPAN));
        self.probe_from = Some(last);
        for offset in first..last {
            match self.fetcher.fetch(offset).await {
                Ok((mut records, probe_high)) => {
                    self.end = self.end.max(probe_high);
                    records.retain(|message| message.offset > self.next);
                    if !records.is_empty() {
                        return Ok(Some(records));
                    }
                }
                Err(FetchError::OffsetOutOfRange) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }

    /// The broker refused the offset: retention may have moved the start past it (jump to the
    /// earliest, counted), otherwise the offset is past the end and the run cannot continue.
    async fn recover(&mut self) -> Result<(), SourceError> {
        let earliest = self.fetcher.earliest().await?;
        let latest = self.fetcher.latest().await?;
        self.end = latest;
        if self.next < earliest {
            metrics::counter!("zirv_flow_kafka_offset_resets_total").increment(1);
            self.next = earliest;
            self.streak = 0;
            self.probe_from = None;
            Ok(())
        } else if self.next > latest {
            Err(code(OFFSET_OUT_OF_RANGE))
        } else {
            Ok(())
        }
    }
}

/// Opens every partition of a topic at the offsets in `positions`.
pub(super) async fn open_all<F: PartitionFetch>(
    fetchers: Vec<(i32, F)>,
    positions: &Positions,
) -> Result<Vec<Partition<F>>, SourceError> {
    let mut opened = Vec::with_capacity(fetchers.len());
    for (id, fetcher) in fetchers {
        let stored = positions.get(id).map(|position| position.next);
        opened.push(Partition::open(id, fetcher, stored).await?);
    }
    Ok(opened)
}

struct ReadState<F> {
    partitions: Vec<Partition<F>>,
    started: Instant,
    budget: Duration,
    stop_when_caught_up: bool,
    idle: Duration,
    finished: bool,
    /// The starting positions were handed over.
    announced: bool,
    /// Keeps the client and the relay alive as long as the stream.
    _keep: Box<dyn Any + Send>,
}

impl<F> ReadState<F> {
    fn positions(&self) -> Positions {
        let mut positions = Positions::default();
        for partition in &self.partitions {
            positions.set(partition.id, partition.position());
        }
        positions
    }
}

/// The batches of a read over `partitions`: first the starting positions, then one per pass that
/// delivered something, a heartbeat after each idle wait, and a last one with the final positions
/// and watermarks. The heartbeats and the first and last batches hold no record. Each batch's checkpoint
/// is the positions of all partitions after it. Ends at the plan's budget, or as soon as every
/// partition is caught up when the plan says so. `keep` is held until the stream is dropped.
pub(super) fn read_partitions<F>(
    partitions: Vec<Partition<F>>,
    plan: ReadPlan,
    idle: Duration,
    keep: Box<dyn Any + Send>,
) -> BoxStream<'static, Result<Batch, SourceError>>
where
    F: PartitionFetch + 'static,
{
    let state = ReadState {
        partitions,
        started: Instant::now(),
        budget: plan.budget.unwrap_or(super::positions::DEFAULT_BUDGET),
        stop_when_caught_up: plan.stop_when_caught_up,
        idle,
        finished: false,
        announced: false,
        _keep: keep,
    };
    stream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        if !state.announced {
            // Where the read starts, before any fetch: the stored offsets, and the earliest
            // offset of a partition that has none. A reader that commits only what it holds
            // needs them to tell the two apart.
            state.announced = true;
            return Some((Ok(final_batch(&state)), state));
        }
        loop {
            let remaining = state.budget.saturating_sub(state.started.elapsed());
            if remaining.is_zero() {
                state.finished = true;
                return Some((Ok(final_batch(&state)), state));
            }
            let mut results = Vec::with_capacity(state.partitions.len());
            for chunk in state.partitions.chunks_mut(IN_FLIGHT) {
                results.extend(
                    join_all(
                        chunk
                            .iter_mut()
                            .map(|partition| async move { (partition.id, partition.pass().await) }),
                    )
                    .await,
                );
            }
            let mut records = Vec::new();
            let mut refused: Vec<Refused> = Vec::new();
            for (id, result) in results {
                match result {
                    Ok(messages) => {
                        for message in &messages {
                            match convert(id, message) {
                                Converted::Record(record) => records.push(record),
                                Converted::Refused(item) => refused.push(item),
                            }
                        }
                    }
                    Err(error) => {
                        state.finished = true;
                        return Some((Err(error), state));
                    }
                }
            }
            let delivered = !records.is_empty() || !refused.is_empty();
            if delivered {
                let checkpoint = state.positions().to_checkpoint();
                return Some((
                    Ok(Batch {
                        records,
                        checkpoint,
                        refused,
                    }),
                    state,
                ));
            }
            if state.stop_when_caught_up && state.partitions.iter().all(Partition::settled) {
                state.finished = true;
                return Some((Ok(final_batch(&state)), state));
            }
            if state.partitions.iter().all(Partition::settled) || state.partitions.is_empty() {
                sleep(state.idle.min(remaining)).await;
                // Nothing came: a heartbeat with the positions and watermarks, so the reader
                // can flush what it holds, report lag and notice the end of its budget.
                return Some((Ok(final_batch(&state)), state));
            }
        }
    })
    .boxed()
}

fn final_batch<F>(state: &ReadState<F>) -> Batch {
    Batch {
        records: Vec::new(),
        checkpoint: state.positions().to_checkpoint(),
        refused: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use chrono::{TimeZone, Utc};
    use rskafka::record::Record as KafkaRecord;

    use super::*;

    fn message(offset: i64) -> RecordAndOffset {
        RecordAndOffset {
            record: KafkaRecord {
                key: Some(format!("k{offset}").into_bytes()),
                value: Some(br#"{"n":1}"#.to_vec()),
                headers: BTreeMap::new(),
                timestamp: Utc.timestamp_millis_opt(0).unwrap(),
            },
            offset,
        }
    }

    type Scripted = Result<(Vec<RecordAndOffset>, i64), FetchError>;

    /// A partition log: offsets with a record, and the earliest and latest offsets. A fetch
    /// returns the records from the requested offset on, at most `page` of them, but none when
    /// the offset is inside a hole (the offsets in `holes` have no record).
    #[derive(Clone)]
    struct Log {
        earliest: i64,
        latest: i64,
        holes: Vec<i64>,
        page: usize,
        calls: Arc<Mutex<Vec<i64>>>,
        /// Fetch results to return before consulting the log.
        script: Arc<Mutex<VecDeque<Scripted>>>,
    }

    impl Log {
        fn new(earliest: i64, latest: i64) -> Self {
            Self {
                earliest,
                latest,
                holes: Vec::new(),
                page: usize::MAX,
                calls: Arc::default(),
                script: Arc::default(),
            }
        }

        fn calls(&self) -> Vec<i64> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PartitionFetch for Log {
        async fn earliest(&self) -> Result<i64, FetchError> {
            Ok(self.earliest)
        }

        async fn latest(&self) -> Result<i64, FetchError> {
            Ok(self.latest)
        }

        async fn fetch(&self, offset: i64) -> Result<(Vec<RecordAndOffset>, i64), FetchError> {
            self.calls.lock().unwrap().push(offset);
            if let Some(next) = self.script.lock().unwrap().pop_front() {
                return next;
            }
            if offset < self.earliest || offset > self.latest {
                return Err(FetchError::OffsetOutOfRange);
            }
            // A hole at the requested offset hides the batch it belongs to: nothing comes back
            // until the request is past the last hole in a row.
            if self.holes.contains(&offset) {
                return Ok((Vec::new(), self.latest));
            }
            let records = (offset..self.latest)
                .filter(|candidate| !self.holes.contains(candidate))
                .take(self.page)
                .map(message)
                .collect();
            Ok((records, self.latest))
        }
    }

    fn offsets(records: &[RecordAndOffset]) -> Vec<i64> {
        records.iter().map(|record| record.offset).collect()
    }

    #[tokio::test]
    async fn a_partition_without_a_stored_offset_starts_at_its_earliest() {
        let partition = Partition::open(0, Log::new(5, 9), None).await.unwrap();

        assert_eq!(partition.position().next, 5);
        assert_eq!(partition.position().end, Some(9));
    }

    #[tokio::test]
    async fn a_stored_offset_is_the_start() {
        let partition = Partition::open(0, Log::new(0, 9), Some(7)).await.unwrap();

        assert_eq!(partition.position().next, 7);
    }

    #[tokio::test]
    async fn a_stored_offset_below_the_log_start_jumps_to_the_earliest_and_is_counted() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let partition = metrics::with_local_recorder(&recorder, || {
            futures_util::FutureExt::now_or_never(Partition::open(0, Log::new(10, 20), Some(3)))
                .unwrap()
                .unwrap()
        });

        assert_eq!(partition.position().next, 10);
        assert!(
            handle
                .render()
                .contains("zirv_flow_kafka_offset_resets_total 1"),
            "{}",
            handle.render()
        );
    }

    #[tokio::test]
    async fn a_stored_offset_above_the_end_fails_the_open() {
        let error = Partition::open(0, Log::new(0, 9), Some(10))
            .await
            .err()
            .unwrap();

        assert_eq!(error, code(OFFSET_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn passes_walk_the_log_and_settle_at_the_end() {
        let mut log = Log::new(0, 5);
        log.page = 2;
        let mut partition = Partition::open(0, log, None).await.unwrap();

        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.extend(offsets(&partition.pass().await.unwrap()));
        }
        assert!(!partition.settled());
        assert!(partition.pass().await.unwrap().is_empty());

        assert_eq!(seen, [0, 1, 2, 3, 4]);
        assert!(partition.settled());
        assert_eq!(partition.position().next, 5);
    }

    #[tokio::test]
    async fn two_empty_fetches_below_the_watermark_probe_past_the_gap_and_jump() {
        // Offset 3 holds a control or compacted-away batch; 4 is a record.
        let mut log = Log::new(0, 8);
        log.holes = vec![3];
        let mut partition = Partition::open(0, log.clone(), Some(3)).await.unwrap();

        let first = partition.pass().await.unwrap();
        assert!(first.is_empty() && !partition.settled());
        let second = partition.pass().await.unwrap();

        assert_eq!(offsets(&second), [4, 5, 6, 7]);
        assert_eq!(partition.position().next, 8);
        assert_eq!(log.calls(), [3, 3, 4]);
    }

    #[tokio::test]
    async fn the_probe_goes_on_past_a_second_undeliverable_offset() {
        // Offsets 3 and 4 hold control batches; 5 is a record.
        let mut log = Log::new(0, 8);
        log.holes = vec![3, 4];
        let mut partition = Partition::open(0, log.clone(), Some(3)).await.unwrap();

        assert!(partition.pass().await.unwrap().is_empty());
        let second = partition.pass().await.unwrap();

        assert_eq!(offsets(&second), [5, 6, 7]);
        assert_eq!(partition.position().next, 8);
        assert_eq!(log.calls(), [3, 3, 4, 5]);
    }

    #[tokio::test]
    async fn a_gap_longer_than_one_probe_is_crossed_over_successive_passes() {
        // Offsets 3 to 19 hold nothing deliverable: the record at 20 is PROBE_SPAN + 1 past 3.
        let mut log = Log::new(0, 40);
        log.holes = (3..20).collect();
        let mut partition = Partition::open(0, log.clone(), Some(3)).await.unwrap();

        assert!(partition.pass().await.unwrap().is_empty());
        let second = partition.pass().await.unwrap();
        assert!(second.is_empty() && partition.settled());
        assert_eq!(partition.position().next, 3, "the offset does not move");
        let third = partition.pass().await.unwrap();

        assert_eq!(offsets(&third), (20..40).collect::<Vec<_>>());
        assert_eq!(partition.position().next, 40);
        // Each pass fetches at `next`, the second then probes 4..=19, the third resumes at 20.
        let mut expected = vec![3, 3];
        expected.extend(4..20);
        expected.extend([3, 20]);
        assert_eq!(log.calls(), expected);
    }

    #[tokio::test]
    async fn a_probe_that_finds_nothing_settles_the_partition_for_this_pass() {
        // An open transaction: the log reaches past `next` but nothing is deliverable.
        let log = Log::new(0, 8);
        log.script
            .lock()
            .unwrap()
            .extend((0..7).map(|_| Ok((Vec::new(), 8))));
        let mut partition = Partition::open(0, log.clone(), Some(2)).await.unwrap();

        assert!(partition.pass().await.unwrap().is_empty());
        assert!(!partition.settled());
        assert!(partition.pass().await.unwrap().is_empty());

        assert!(partition.settled());
        assert_eq!(partition.position().next, 2, "the offset does not move");
        assert_eq!(log.calls(), [2, 2, 3, 4, 5, 6, 7]);
    }

    #[tokio::test]
    async fn an_offset_the_broker_refuses_below_the_start_jumps_and_above_the_end_fails() {
        let log = Log::new(0, 9);
        log.script
            .lock()
            .unwrap()
            .push_back(Err(FetchError::OffsetOutOfRange));
        let mut moved = Partition::open(0, Log::new(0, 9), Some(4)).await.unwrap();
        moved.fetcher = Log::new(6, 9);
        moved
            .fetcher
            .script
            .lock()
            .unwrap()
            .push_back(Err(FetchError::OffsetOutOfRange));
        assert!(moved.pass().await.unwrap().is_empty());
        assert_eq!(moved.position().next, 6);

        let mut beyond = Partition::open(0, log.clone(), Some(4)).await.unwrap();
        beyond.fetcher = Log::new(0, 3);
        beyond
            .fetcher
            .script
            .lock()
            .unwrap()
            .push_back(Err(FetchError::OffsetOutOfRange));
        let error = beyond.pass().await.unwrap_err();

        assert_eq!(error, code(OFFSET_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn a_fetch_failure_is_reported_as_its_code() {
        let log = Log::new(0, 9);
        log.script
            .lock()
            .unwrap()
            .push_back(Err(FetchError::Failed("kafka_timeout")));
        let mut partition = Partition::open(0, log, None).await.unwrap();

        let error = partition.pass().await.unwrap_err();

        assert_eq!(error, code("kafka_timeout"));
    }

    async fn collect(
        logs: Vec<(i32, Log)>,
        positions: Positions,
        plan: ReadPlan,
    ) -> Vec<Result<Batch, SourceError>> {
        let partitions = open_all(logs, &positions).await.unwrap();
        read_partitions(partitions, plan, Duration::from_millis(5), Box::new(()))
            .collect()
            .await
    }

    #[tokio::test]
    async fn a_bounded_read_yields_what_is_there_then_a_final_checkpoint_and_ends() {
        let batches = collect(
            vec![(0, Log::new(0, 3)), (1, Log::new(0, 2))],
            Positions::default(),
            ReadPlan::default(),
        )
        .await;

        let batches: Vec<Batch> = batches.into_iter().map(Result::unwrap).collect();
        let keys: Vec<_> = batches
            .iter()
            .flat_map(|batch| batch.records.iter().map(|record| record.key.clone()))
            .collect();
        assert_eq!(keys, ["k0", "k1", "k2", "k0", "k1"]);
        let last = batches.last().unwrap();
        assert!(last.records.is_empty());
        let positions = Positions::from_checkpoint(&last.checkpoint).unwrap();
        assert_eq!(positions.get(0).unwrap().next, 3);
        assert_eq!(positions.get(1).unwrap().next, 2);
        assert_eq!(positions.get(1).unwrap().end, Some(2));
    }

    #[tokio::test]
    async fn a_resumed_read_starts_at_the_stored_offsets_and_a_new_partition_at_the_earliest() {
        let mut stored = Positions::default();
        stored.set(0, PartitionPosition { next: 2, end: None });

        let batches = collect(
            vec![(0, Log::new(0, 4)), (1, Log::new(1, 3))],
            stored,
            ReadPlan::default(),
        )
        .await;

        let occurrences: Vec<_> = batches
            .into_iter()
            .flat_map(|batch| batch.unwrap().records)
            .map(|record| record.occurrence.unwrap())
            .collect();
        assert_eq!(occurrences, ["0/2", "0/3", "1/1", "1/2"]);
    }

    #[tokio::test]
    async fn the_first_batch_holds_the_starting_positions_and_no_record() {
        let mut stored = Positions::default();
        stored.set(0, PartitionPosition { next: 2, end: None });

        let batches = collect(
            vec![(0, Log::new(0, 4)), (1, Log::new(1, 3))],
            stored,
            ReadPlan::default(),
        )
        .await;

        let first = batches[0].as_ref().unwrap();
        assert!(first.records.is_empty() && first.refused.is_empty());
        let positions = Positions::from_checkpoint(&first.checkpoint).unwrap();
        assert_eq!(positions.get(0).unwrap().next, 2, "the stored offset");
        assert_eq!(
            positions.get(1).unwrap().next,
            1,
            "the earliest of a new partition"
        );
    }

    #[tokio::test]
    async fn a_malformed_message_is_refused_and_the_partition_goes_on() {
        let log = Log::new(0, 3);
        let mut bad = message(1);
        bad.record.value = Some(b"{nope".to_vec());
        log.script
            .lock()
            .unwrap()
            .push_back(Ok((vec![message(0), bad, message(2)], 3)));

        let batches = collect(vec![(7, log)], Positions::default(), ReadPlan::default()).await;

        assert!(
            batches[0].as_ref().unwrap().records.is_empty(),
            "the starting positions come first"
        );
        let first = batches[1].as_ref().unwrap();
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.refused.len(), 1);
        assert_eq!(first.refused[0].location, "partition 7 offset 1");
        assert_eq!(first.refused[0].code, "kafka_value_not_json");
        let positions = Positions::from_checkpoint(&first.checkpoint).unwrap();
        assert_eq!(positions.get(7).unwrap().next, 3);
    }

    #[tokio::test]
    async fn a_read_that_waits_for_more_ends_at_its_budget() {
        let plan = ReadPlan {
            budget: Some(Duration::from_millis(60)),
            stop_when_caught_up: false,
        };
        let started = std::time::Instant::now();

        let batches = collect(vec![(0, Log::new(0, 0))], Positions::default(), plan).await;

        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Idle passes beat, and the last batch is the final positions; none holds a record.
        assert!(batches.len() >= 2, "{}", batches.len());
        assert!(
            batches
                .iter()
                .all(|batch| batch.as_ref().unwrap().records.is_empty())
        );
    }

    #[tokio::test]
    async fn a_failing_partition_ends_the_read_with_its_code() {
        let log = Log::new(0, 3);
        log.script
            .lock()
            .unwrap()
            .push_back(Err(FetchError::Failed("kafka_timeout")));

        let batches = collect(vec![(0, log)], Positions::default(), ReadPlan::default()).await;

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[1].as_ref().unwrap_err(), &code("kafka_timeout"));
    }
}
