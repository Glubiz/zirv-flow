# M8 validation: scalability envelope (section 19)

This report validates the [section 19 envelope](../architecture.md#19-scalability-envelope) for issue
[#397](https://github.com/Glubiz/zirv-flow/issues/397). The gate is
[check-report.sh](../../scripts/bench/m8/check-report.sh). Every figure below is a
reference-resource emulation, not a reference-hardware run (ruling ef99d178). Validation on dedicated reference
hardware remains outstanding.

On 2026-11-05 the operator stopped the benchmark and called M8 complete ("Stop the benchmark and call m8 completed").
Every target still missed at that point is `revised by decision` below, with its measured value and what to expect at
that size. Some rows rest on the run log because the raw run file was overwritten by a later run of the same script or
is a `.failed.json` file, which the gate refuses to cite; those files are kept in [data/m8](data/m8/) as evidence.

## 1. Setup

| Item | Value |
| --- | --- |
| PostgreSQL | 16.15, 8 cores (cpuset 8-15), 20 GiB memory limit, 6 GiB shared memory, `shared_buffers` 5 GB, `wal_compression` lz4, `max_wal_size` 32 GB, `max_connections` 400 |
| `serve` | 4 cores, 5 GiB; main pool 32, explorer pool 4 |
| `worker` and `scheduler` | 4 cores together (worker 5 GiB, scheduler 1 GiB) |
| Host | One Docker VM of about 33.5 GB, not the reference hardware. The reference allocation is PostgreSQL 32 GB and two app hosts of 4 cores; here memory is scaled to 31 GiB of caps (a recorded deviation from ef99d178, because the operator ruled not to raise the VM memory) and there is one app container pair |
| Data | 25,000,000 people (10,000,000 in the benchmark workspace), 90,700,000 seeded events (about 146 M at the last explorer run, because every ingest run adds events), 10,000,000 waiting enrollments |
| Not reached | 250 M and 500 M retained events. The operator stopped the events seed at about 87 M on 2026-11-02 (it had slowed to 2.3 k rows/s) |
| Seed integrity | Three PostgreSQL backend crashes (signal 11) during seeding; an amcheck over 515 btree indexes was clean afterwards; none occurred during measurement |

Images per round (the `image_version` of each data file):

| Round | Date | Image | What ran |
| --- | --- | --- | --- |
| 1 | 2026-11-02 | 0.214.0 | Ingest, render, timers at concurrency 4 |
| 3 | 2026-11-03 | 0.222.0 (timers 0.225.0) | Debugger, render, segments, jobs, decisions A/B, timers |
| 4a | 2026-11-04 | 0.226.0 | Send sweep, decisions A/B, explorer split, segments incremental |
| 4b | 2026-11-04 | 0.230.0 | Ingest, 500-item batch, explorer at 1, 4 and 16 clients, jobs (timers aborted) |
| 4c | 2026-11-05 | 0.234.0 | Jobs, ingest with inbox-to-applied, burst, explorer (timers stopped by the operator) |

**Host-overlap caveat.** The CPU was capped for heat, and the Docker VM was pinned to the efficiency cores. Almost every
capped window overlapped a host cargo task (builds, test PostgreSQL instances), each logged as a LIMITS line in the run
log. Storage, network and the host scheduler were shared with that work, so read single runs as having a spread:
ingest-alone p99 readings on the same day ranged from 133 to 388 ms.

## 2. Results per dimension

The measured column is the latest valid measurement and its image. "Data file" links the cited run (smoke false, exit
code 0).

| Dimension | Target | Measured (image) | Data file | Verdict |
| --- | --- | --- | --- | --- |
| People per workspace / per deployment | 10 million / 25 million | 25,000,000 people seeded and used by every run; 10,000,000 in the benchmark workspace (0.234.0) | [ingest-4c.json](data/m8/ingest-4c.json) | met |
| Retained events per deployment | 500 million rows | 90.7 M seeded, about 146 M at the last explorer run (0.234.0); 500 M not reached | [ingest-4c.json](data/m8/ingest-4c.json) | revised by decision |
| Sustained / burst ingestion | 2,000 events/s / 10,000 events/s for 10 min, backlog drained within 15 min | Sustained 2,000/s held with 0 rejections. Burst 10,000/s for 600 s held, 6,000,000 accepted, backlog peak 4,039,200 rows, drain 1,506 s against 900 s (0.234.0) | [burst.json](data/m8/burst.json) | revised by decision |
| Ingestion API latency | p99 < 100 ms at sustained rate | p99 9.5 ms at 2,000/s (0.234.0); 500-item batch p99 84.6 ms (0.230.0). Earlier rounds: 416, 249, 427 and 262 ms | [ingest-4c.json](data/m8/ingest-4c.json), [batch-4b.json](data/m8/batch-4b.json) | revised by decision |
| Inbox to applied profile | p95 < 5 s at sustained rate | p50 30.4 ms, p95 65.4 ms, p99 108.1 ms (0.234.0) | [ingest-4c.json](data/m8/ingest-4c.json) | met |
| Live debugger delivery from durable inbox append | p95 < 2 s at sustained ingestion rate | 1 client p95 222.5 ms; 20 clients p95 651.5 ms (0.222.0), over delivered frames: 1 client received 88.9 % of events, each of 20 clients about 21 %; the rest were skipped as gap frames | [debugger-1client.json](data/m8/debugger-1client.json), [debugger-20clients.json](data/m8/debugger-20clients.json) | met |
| Event explorer interactive query | p95 < 5 s for a 90-day filtered count or property breakdown; hard timeout 15 s | 1 client: slowest shape p95 4.9 s, ingest held 2,000/s (0.226.0, valid file). At 4 clients every shape p95 was under 4.3 s with 0 timeouts; at 16 clients about 83 % of requests answered `explorer_busy`, breakdown p95 5.2 s with 30 query timeouts, and both admitted exports ran past the 300 s client timeout (0.230.0 and 0.234.0, `.failed.json` files) | [explorer.json](data/m8/explorer.json) | revised by decision |
| Decision-log append overhead | p95 < 5 ms added and < 10 % ingestion-throughput loss | Mean throughput loss 4.6 % (rounds 31.1 %, -41.5 %, 11.6 %: noise-limited); added p95 19.4 ms (0.226.0, before the `event.accepted` decisions were dropped in 0.231.0; not re-measured) | [decisions-4a.json](data/m8/decisions-4a.json) | revised by decision |
| Segment preview, 10 M people | p95 < 3 s trait-only; < 15 s with a 90-day event aggregate | Trait-only: jsonb p95 3.9 s, trait slots p95 2.2 s. Aggregate: count p95 21.0 s, sum by category p95 5.4 s (0.222.0, from the run log; the raw file was overwritten) | [segments.json](data/m8/segments.json) | revised by decision |
| Full segment refresh, 10 M people | < 2 min trait-only; < 10 min with event aggregates | Median 118 s trait-only, 177 s with an aggregate (0.222.0, from the run log) | [segments.json](data/m8/segments.json) | met |
| Event to segment entry to enrollment | p95 < 60 s | Event to `segment.entered` p50 8.6 s, p95 22.0 s, p99 27.2 s over 50,899 entries at 2,000 events/s (0.226.0). The enrollment leg was not measured | [segments-4a.json](data/m8/segments-4a.json) | revised by decision |
| Waiting enrollments; timer lateness | 10 million; p99 < 60 s at that volume | All 10,000,000 woke. Lateness p99 1,401 s with no load and 2,304 s at 2,000 events/s (0.225.0). The batched-wake changes of PR #511 (0.234.0) were not re-measured at 10 M | [timers.json](data/m8/timers.json) | revised by decision |
| Job throughput | 1,000 jobs/s claimed and completed | Uncapped 9,924/s at concurrency 16 and 11,871/s at 32. With `worker.max_running_per_workspace` set: cap 4 1,215/s at concurrency 16, cap 16 2,316/s at concurrency 32 (0.234.0). Capped claims had been 849/s (0.222.0), stalled (0.226.0) and 502/s (0.230.0) | [jobs-4c-c16.json](data/m8/jobs-4c-c16.json), [jobs-4c-c32.json](data/m8/jobs-4c-c32.json) | missed and fixed |
| Send throughput | 300 messages/s per deployment | 164.7 msg/s at the default concurrency 4 (0.222.0); 289.2 at 8, 313.2 at 12, 320.9 at 16 (0.226.0, 60,000 people, worker CPU 299 % at 16) | [send-c16.json](data/m8/send-c16.json), [send-c12.json](data/m8/send-c12.json) | missed and fixed |
| Template render, typical block email (about 20 blocks, conditions pre-evaluated) | p95 < 50 ms per message | p95 1.72 ms, p99 1.86 ms at pool size 2 (renderbench, a host tool: Docker limits do not apply) | [render.json](data/m8/render.json) | met |

Fix references for `missed and fixed`:

- **Job throughput.** The capped claim stalled on 0.226.0 (a livelock: batches were given back because the hold was
  counted from `sent`). PR #506 (`perf/timers2`, 0.230.0) leased capped claims from their update, counted the hold from
  the claim's return and bounded the claim's probes; PR #509 (`perf/capped-claim`, 0.233.0) woke an idle slot for each
  job that leaves running under a cap. The measurements that show it are
  [jobs-4c-c16.json](data/m8/jobs-4c-c16.json) (cap 4 ran at 1,215/s over 100,001 jobs with no stall) and
  [jobs-4c-c32.json](data/m8/jobs-4c-c32.json) (cap 16 ran at 2,316/s).
- **Send throughput.** No code change. The default `worker.concurrency` stays 4; the target is met by raising it to 12 to
  16 ([send-c12.json](data/m8/send-c12.json), [send-c16.json](data/m8/send-c16.json)), now stated in
  [sizing](../operations/sizing.md). At 16 the worker runs at its 300 % CPU limit and a queue still stands (claim lag
  p95 3.9 s), so there is no headroom beyond about 320 messages/s on 4 worker cores.

## 3. Revised by decision

Each item below is covered by the operator decision of 2026-11-05 (stop the benchmark, call M8 complete) and by the
earlier rulings named. The limit is stated in plain terms for an operator at the benchmark size.

- **Retained events (500 M).** Operator, 2026-11-02: stop the seed at about 87 M (superseding the supervisor's seed
  time-box ruling of 2026-11-01). Measured: 90.7 M seeded, about 146 M at the last run. Expect: nothing beyond about
  150 M events is validated here, and explorer and segment latencies grow with the event count (the explorer's slowest
  one-client p95 went from 3.4 s at about 95 M events to 4.9 s at about 128 M). The 250 M and 500 M points go to the
  reference-hardware run.
- **Burst drain.** Ruling 3fc88974 (drop the per-event `event.accepted` decision, shipped in 0.231.0, after the burst
  design) and the decision of 2026-11-05. Measured: after a 10-minute burst at 10,000/s the backlog peaked at 4.04 M
  rows and drained in 1,506 s against 900 s. Expect: the API absorbs 10,000/s for 10 minutes (p99 153.7 ms, no 429 or
  5xx), but profiles catch up about 25 minutes after the burst ends. At the sustained 2,000/s, inbox-to-applied p95
  stays near 65 ms.
- **Ingest p99 tail.** Ruling 07180bb5 (investigation H) and the decision of 2026-11-05. The latest run met the target at
  9.5 ms, but earlier rounds on the same hardware measured 249 to 427 ms. The slow seconds come from PostgreSQL write
  stalls (one WAL fdatasync of 0.1 to 1.2 s queues every serve connection in COMMIT); the settings A/B that would test a
  remedy was not run. Expect: a p99 under 100 ms on a quiet database and a p99 of a few hundred milliseconds in
  checkpoint and write bursts. Size the `serve` main pool as in [sizing](../operations/sizing.md).
- **Explorer.** Rulings e2c3bf14 (`explorer.max_exports`, default 1) and 4e2aa7b2 (separate explorer pool of 4) and the
  decision of 2026-11-05. The ingest collapse under concurrent exports is fixed: ingest held 2,000/s at 4 and 16 clients
  on 0.230.0 and 0.234.0. Remaining limits: one export at a time; at 16 explorer clients the one export slot and the
  four-connection pool leave about 83 % of requests answering `explorer_busy`, and an admitted 1 M-row export can run
  past 300 s. The 0.234.0 exports were about twice as slow as on 0.230.0 (p50 25.5 s against 13.0 s at 1 client;
  slowest 167 s) because a per-page decision lookup added after 0.230.0 cost about 235 ms per 10,000-row page. The
  export path no longer runs that lookup (0.235.0). That fix is verified by tests and **not re-benched**, so the
  expected one-client export time is the 0.230.0 value (about 13 s for 1 M rows), unconfirmed. The 15 s interactive
  timeout arm never ran; the shipped timeout stays 5 s. Expect: 4 clients are comfortable; at 16 clients exports are
  unreliable and interactive queries often answer `explorer_busy`.
- **Decision-log overhead.** Ruling 3fc88974 and the decision of 2026-11-05. The 4.6 % throughput-loss mean passes, but
  single rounds ranged from -41.5 % to 31.1 %, so it is not a reliable pass; the added p95 was 19.4 ms against 5 ms and
  WAL grew 47 % per run. Dropping `event.accepted` (0.231.0) removes 86 % of the decision rows; the effect was not
  re-measured. Expect: decision writes cost a measurable share of drain throughput and WAL.
- **Segment previews.** Ruling 57ff9ef1 (park `perf/segment-preview` unmerged) and ruling f2af5ae5 (covering index, one
  statement for count plus sample, no JIT; not merged). Measured: trait-only preview p95 3.9 s (jsonb) and 2.2 s (slots)
  against 3 s; aggregate count preview p95 21.0 s against 15 s (503 at the 20 s warm-up limit). Expect: previews on a
  10 M-person workspace take a few seconds for trait filters and about 20 s for an event-aggregate count, and the first
  aggregate previews after a restart can answer 503. Slots are faster than jsonb. Backfilling a trait slot over 10 M
  people took 1,839 to 2,102 s.
- **Event to segment entry to enrollment.** Measured entry leg p95 22.0 s against 60 s; the enrollment leg was not
  measured. Expect: a segment entry within about 30 s of the event at 2,000 events/s; the time to enrollment is unmeasured.
- **Timers at 10 M.** Rulings 91fd5fa7 (batch claims and batch wakes), 48be9592, 98e6546e and f469f70c, and the decision
  of 2026-11-05. Measured (0.225.0, concurrency 32): p99 1,401 s with no load and 2,304 s at 2,000 events/s, after
  about 480 wakes/s at concurrency 4 (p99 5,267 s). Waking 10 M enrollments over 40 minutes needs about 4,200 wakes/s;
  the drain ran at about 2,600 to 3,000/s. Investigation G puts the loaded case out of reach on 8 PostgreSQL cores
  (about 12 to 16 cores and 6 to 8 worker cores would be needed). Expect: a mass wake of millions of enrollments in one
  period is drained at about 2,600 to 3,000 wakes/s, so wake lateness grows with the backlog; at 1 million waiting
  enrollments the [M5](m5-flows.md) result (p99 52 s unloaded) is the guide. PR #511 (timers3, 0.234.0) cut the round
  trips per wake from 15.3 to 12.4 in a test; its 10 M effect is unmeasured.
- **Capped-job history.** Job throughput is `missed and fixed`, not revised; the earlier capped results (849/s, stall,
  502/s) are history. The capped claim still serialises on an advisory lock (mean wait 27 to 73 ms) and runs at 1,215 to
  2,316 jobs/s, so ruling 56c4341e's serialization redesign was not needed.

## 4. Not settled

- Timers3 at 10 M, the PostgreSQL settings A/B for the ingest tail, and the segment-preview changes have no measurement.
- Explorer exports on 0.235.0 and the 16-client export timeout after that fix were not re-benched.
- The enrollment leg of "event to segment entry to enrollment" was not measured.
- Dedicated reference hardware, and the 250 M and 500 M event points, remain outstanding.
