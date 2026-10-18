# M4 segment benchmark

Issue #127: architecture section 19 targets for segment preview, full refresh and incremental
re-evaluation at 10 million people and 50 million events, and the data for OQ-2. Measured with the
published image `ghcr.io/glubiz/zirv-flow:0.56.0` (main after #126), digest
`sha256:0b429e9f3dee4b652076429773539c10a39fc47121003c31f54cdb575039b7cd`. The smoke run of the
scripts used 0.55.0 and is not a result. Raw JSON and plans are in
[`data/m4/`](data/m4/); the scripts are in `scripts/bench/m4/` (runbook: `scripts/bench/README.md`
section 9).

**This is a development machine, not reference hardware.** Every figure is one host with Docker
Desktop. A target missed here is provisional (spec D1) and is not revised from this run alone.
Where a miss below is marked **code-caused**, a defect in the product's queries explains it, not the
hardware, and the re-check on reference hardware does not apply to it until the defect is fixed.

## 1. Summary

As shipped means the 0.56.0 image with no change of settings. "Custom plan" is the same stack with
`ALTER DATABASE ... SET plan_cache_mode = force_custom_plan` (section 4.1), kept only as evidence of
what per-execution planning buys; the settings were reset afterwards.

| Target (section 19) | Measured as shipped | With `force_custom_plan` | |
|---|---|---|---|
| Preview, trait-only, p95 < 3 s, on `jsonb` (D4.1) | p50 5.52 s, p95 5.57 s (20 runs) | not run (the mirror statement counts in 1.55 s) | MISS, code-caused |
| Preview, trait-only, p95 < 3 s, on slots (D4.3) | p50 6.04 s, p95 6.14 s | p50 0.97 s, p95 1.01 s | MISS as shipped, code-caused; HIT with per-execution planning |
| Preview with a 90-day aggregate, p95 < 15 s (D4.4) | 20 of 20 previews 503 at the fixed 10 s limit; the query takes 38.7 s uncapped | 20 of 20 503; 37.7 s uncapped | MISS, code-caused |
| Preview, property-filtered aggregate, p95 < 15 s (D4.5) | 20 of 20 503 at 10 s; 97.5 s uncapped | 20 of 20 503; 98.0 s uncapped | MISS, code-caused |
| Full refresh, trait-only, < 2 min (D4.6) | median 77.6 s for the first refresh of a new segment (5 runs: 77.6, 78.6, 78.9, 40.0, 41.4); a manual refresh of it 5.4 s | not run | HIT |
| Full refresh with an aggregate, < 10 min (D4.6) | median 57.6 s first refresh (5 runs: 50.7 to 70.2); manual 45.0 s | not run | HIT |
| Event to `segment.entered`, p95 < 60 s (D4.7) | p95 0.382 s over 22,084 entries (p50 0.044 s, p99 9.7 s, max 13.3 s) at 2,000 events/s for 10 min | not run | HIT |
| Ingest cost of the change marks (D4.8) | drain medians 79,586 ms against 76,286 ms, +4.3%, interleaved on this machine (#122, PR #137); cited, not a new run | | cited |

Index backfills (D4.2, a data point, not a target) at 10 M people: `plan` 1,002.8 s, `age` 1,089.6 s,
`marketing_opt_in` 1,133.7 s, one after another (`trait_s1`, `trait_n1`, `trait_b1`).

## 2. Setup

### 2.1 Hardware (spec D1)

The reference hardware is PostgreSQL on 8 vCPU / 32 GB / NVMe and two app hosts of 4 vCPU. This run
used (`data/m4/stack.txt`):

- Host: 13th Gen Intel Core i9-13900K, Windows, Docker Desktop 29.2.1 (32 CPUs, 31 GiB for the VM),
  WSL 3.0.3.0.
- **PostgreSQL 16.15** in its own cpuset `8-31`, no memory limit, `shared_buffers=4GB`,
  `effective_cache_size=16GB`, `maintenance_work_mem=1GB`, `max_wal_size=8GB`,
  `checkpoint_timeout=15min`, everything else stock (`max_parallel_workers_per_gather=2`,
  `random_page_cost=4`). **`/dev/shm` is 8 GB** (`BENCH_PG_SHM=8gb`): the default 64 MB, and the
  1 GB first used, ran out during a parallel vacuum, so the container was recreated on the same
  volumes with 8 GB before any measurement.
- **zirv-flow at tier M**: cpuset `0-3` shared by serve, worker and scheduler, memory 3277 / 4096 /
  819 MB (serve / worker / scheduler), worker concurrency 4, `ingest.drain_shards=8`,
  `log.filter=warn`. loadgen runs on the host.
- Compose project `zirv-flow-m4bench10m`, image run with `--no-build` (never built locally).

### 2.2 Data (spec D3)

One workspace, written by SQL into the real tables after the app migrated the database
(`scripts/bench/m4/seed.sql`, `setseed(0.42)`), with the trait definitions and event schemas created
through the API. The stored shapes were checked against an identify and a track sent through the API.

| | Rows | Table | Indexes |
|---|---|---|---|
| `people` | 10,000,000 | 10.2 GB | 2.5 GB, of which the GIN `jsonb_path_ops` on `traits` 1.4 GB (slot indexes built later) |
| `identities` | 1,000,000 (the loadgen pool, `bench-u-0` to `bench-u-999999`) | 117 MB | 222 MB |
| `events` | 50,000,000 over 365 days, 0 in the default partition | 10.1 GB | 12.1 GB |

Events by name: `bench_page_viewed` 20,004,843, `bench_order_placed` 12,494,756,
`bench_item_added` 9,999,113, `bench_support_opened` 4,997,947, `bench_plan_changed` 2,503,341. Each
person has 20 mixed-type traits (the OQ-1 shape). 50 M events in 365 days is 10% of the 500 M
retained-event envelope of section 19. The seed ran from 04:52:13 UTC; the background command was
cut by a one-hour cap at 05:52 with every event committed (exact counts above), so the load took at
most 3,600 s; `VACUUM (ANALYZE, PARALLEL 0)` and the size manifest ran as separate commands.

**Deviations from the issue:** the event is `bench_order_placed`, not `order_placed`, because
`loadgen` hard-codes the five `bench_*` names and the incremental run needs its events to land in the
aggregate. The incremental segment uses the count aggregate: loadgen sends no `category`. The
loadgen pool is 1,000,000 people (`--people 1000000`).

### 2.3 Definitions (`scripts/bench/m4/defs/`)

- Trait-only: `plan eq pro` and `age between 25 and 45` and `marketing_opt_in eq true`: 677,521
  people. Its traits are the three indexed in D4.2.
- Aggregate: `plan eq pro` and `bench_order_placed` count >= 2 in the last 90 days (P90D): 128,399
  people at seed time.
- Property-filtered aggregate: sum of `amount` > 150.005 over `bench_order_placed` with
  `category eq books` in 90 days: 77,509 people.

The preview count of the trait-only definition equals a hand-written SQL count over the seeded rows
(677,521; `verify.json`). The two aggregate previews return 503, so their counts could not be compared
through the API at this size (the smoke run, 100,000 people, compared all three and they matched).

### 2.4 Method

Every measurement goes through the HTTP API as the bench owner. Preview: one warm-up and 20 timed
`POST .../segments/preview` calls (sample size 20, the default), p50 the median and p95 by nearest
rank, of curl's `time_total`. A preview that returns 503 is recorded with its status, and the statement is then
timed uncapped under `EXPLAIN (ANALYZE, BUFFERS)` through a hand-written mirror of the compiled SQL
(`explain.sql`; the store cannot print the compiler's SQL, so the mirror's count is checked against the
API's). Refresh: five fresh segments, each timed from the refresh job's `created_at` to the poll that
no longer finds the job (0.3 s poll, `total_s`), first (the save-triggered initial refresh, every
member enters) and manual (`POST .../refresh`, a full recompute that diffs against the open rows).
Incremental: a saved aggregate segment refreshed first, then `loadgen` at 2,000 track events/s for
600 s (1,199,999 accepted, 0 late, API p99 60 ms), until the inbox and dirty marks were empty, then
`incremental-latency.sql`: `decision_records.recorded_at` of each event-caused `segment.entered`
minus the causing event's `received_at` (the transaction start, a slight under-estimate).

## 3. Results

### 3.1 Trait-only preview and the slots

| Run | p50 | p95 | Notes |
|---|---|---|---|
| `jsonb` (D4.1) | 5.52 s | 5.57 s | `min` 1.58 s. The mirror statement runs in 1.55 s (parallel scan) |
| Slots right after `ready`, before `ANALYZE` | 6.40 s | 6.87 s | index scan on `people_trait_b1_idx`, row estimate 1 (actual 677,521); 3.8 s for the mirror |
| Slots after `ANALYZE people` (2 s), autovacuum still running | 5.93 s | 6.31 s | the mirror now plans a parallel seq scan, 1.07 s |
| Slots, quiet database (headline) | 6.04 s | 6.14 s | |
| Slots, `force_custom_plan` | 0.97 s | 1.01 s | |

The slot indexes are used (`explain_indexes` and `*.explain.txt`), but they are not what decides the
time. Two things were found while explaining why the API is five seconds slower than the same count
run by hand (`preview-slots.api-sql.explain.txt`, `preview-slots.api-statements.log`):

1. **Generic plan.** The API runs the compiled count as a persistent prepared statement (sqlx). After
   five executions on a connection PostgreSQL switches to a generic plan, which cannot see the bound
   values and so estimates 7,362 rows for the 677,521 that match. It picks a non-parallel index scan
   on one slot (5.9 to 6.2 s) instead of the parallel sequential scan of the first five executions
   (1.0 s). Reproduced in psql with the logged statement text: executions 1 to 5 run the parallel
   plan, the sixth the index scan. The same flip is why `jsonb` is bimodal (min 1.58 s, p50 5.5 s). At
   the 100,000-person smoke size it is invisible. **Code-caused.**
2. **Stale statistics.** The backfill does not `ANALYZE`, so right after `ready` the new slot columns
   have no statistics (`trait_b1`: all null) and the estimate is 1 row. `ANALYZE people` takes 2 s at 10 M
   rows. After it the custom plan is correct; the generic plan is not, so the two fixes are needed
   together. The mid-backfill autovacuum of `people` also ran for several minutes afterwards and slows
   previews while it does; the headline is from a quiet database. **Code-caused.**

The 20-row sample statement takes 0.3 ms and is not part of the gap.

### 3.2 Aggregate previews and OQ-2

Both aggregate previews are 503 (`segment_preview_timeout`) on 20 of 20 runs, as shipped and with
`force_custom_plan`. The API's statement timeout is a fixed 10 s (`PREVIEW_TIMEOUT`, no setting), so
the 15 s target cannot be met by any query slower than 10 s; the uncapped times are the measured
misses:

| Definition | Uncapped, as shipped | Uncapped, custom plan | Target |
|---|---|---|---|
| Aggregate (D4.4) | 38.7 s | 37.7 s | 15 s |
| Property-filtered aggregate (D4.5) | 97.5 s | 98.0 s | 15 s |

The plan is a correlated scalar subquery per candidate person (`COALESCE((SELECT count(*) FROM events e
WHERE e.person_id = p.id AND ...) >= 2, false)`), which PostgreSQL does not decorrelate: a SubPlan runs
once per person that survives the cheaper predicates (a probe of the four partitions that cover 90
days each time), and for the sum with no other trait condition it runs for every one of the 10 M
people. Neither the plan cache mode nor the slots change that. The full refresh runs the same SQL with
no 10 s limit, which is why it takes 45 s (aggregate) while the preview 503s.

### 3.3 Refresh and incremental

The trait-only first refresh (every one of 677,521 members enters, with an event and a decision row
each) takes 77.6 s at the median, inside 2 min; runs 4 and 5 took about 40 s for a reason not
identified here (possibly a plan difference on the worker's connection). A manual refresh, where
nothing changes, is 5.4 s. The aggregate refresh is 57.6 s at the median (the compile plus the 45 s
query), inside 10 min. Incremental: 22,084 event-caused entries into the aggregate segment (128,230
members before, 150,313 after, 1 exit), p95 0.382 s, p99 9.7 s, max 13.3 s, target 60 s. The inbox and
the marks were empty after the drain wait that follows the loadgen run. The refresh and re-evaluation jobs were measured as
shipped only.

## 4. Decisions and follow-ups

### 4.1 OQ-2: open

The measured gap is above: previews with a 90-day aggregate take 38.7 s and a property-filtered one
97.5 s uncapped against a 15 s target, and 503 at the 10 s limit. The cause is the compiler's
correlated subquery per person, not the absence of a rollup. OQ-2 stays open (supervisor ruling
b54f3196). The next step: reshape the compiler first, to emit a grouped set over events (workspace,
name, window, property filters, `GROUP BY person_id HAVING agg op N`) semi-joined to `people`, with a
`LEFT JOIN`/`COALESCE` form for comparisons that zero-event people satisfy, then re-bench at the same
10 M / 50 M. "No rollup for v1" is decided only if both aggregate cases then meet 15 s; otherwise a
rollup design is required and it must cover property-filtered aggregates.

### 4.2 Missed targets

| Miss | Gap | Cause | What closes it |
|---|---|---|---|
| Trait-only preview on `jsonb` and on slots | 5.57 s and 6.14 s against 3 s | code-caused: generic plan of the persistent prepared statement; no `ANALYZE` after a backfill | `.persistent(false)` on the compiled segment count, sample and membership queries; `ANALYZE people` when a slot backfill reaches `ready`. With per-execution planning the slots reach 1.01 s |
| Aggregate previews | 38.7 s and 97.5 s uncapped; 503 at 10 s | code-caused: correlated subquery per person | the compiler reshape of 4.1, then a rollup if that is not enough |
| 10 s limit against the 15 s target | 503 even at 10 to 15 s | design inconsistency: architecture 8.2 fixes 10 s, section 19 targets 15 s | a fixed 20 s limit (ruling 9201125b) |

What stays provisional under D1 (hardware pending M8) is whatever remains after those fixes. Section
19's fallbacks (name-level dirty filtering, `database.read_url`, rollups) are not needed for the
passes above.

### 4.3 Follow-up issues

**A (#143). Segment queries lose their parallel plan after five executions (generic plan), and slot backfills leave no statistics**

At 10 M people the slotted trait-only preview takes p95 6.14 s as shipped (target 3 s) and 1.01 s with
`plan_cache_mode = force_custom_plan` (D4.3). The compiled count is a persistent sqlx prepared
statement; after about five executions on a connection PostgreSQL uses a generic plan that estimates
7,362 rows for 677,521 and runs a non-parallel index scan (5.9 s) instead of the parallel sequential
scan (1.0 s). The `jsonb` preview is bimodal for the same reason (min 1.58 s, p50 5.52 s). Separately,
a slot backfill does not `ANALYZE`, so the new columns have no statistics (estimate 1 row) until
autovacuum. Fix: `.persistent(false)` on the compiled segment count, sample and membership queries
only (not `plan_cache_mode` in `begin_snapshot_for`), and `ANALYZE people` when a backfill reaches
`ready`. Check whether the refresh and re-evaluation queries share it: the refresh runs once per job,
and the trait-only first refresh varied between 78 s and 40 s across runs (unexplained). Evidence:
`docs/benchmarks/data/m4/preview-slots*.json`, `preview-slots.api-sql.explain.txt`.

**B (#144). Segment aggregate predicates are a correlated subquery per person (OQ-2)**

Compile event aggregates as a grouped set over events (workspace, name, window, property filters,
`GROUP BY person_id HAVING agg op N`) semi-joined to `people`, with a `LEFT JOIN`/`COALESCE` form for
comparisons that zero-event people satisfy. At 10 M people / 50 M events a 90-day count aggregate takes
38.7 s and a property-filtered sum 97.5 s uncapped (targets 15 s; both 503 at the 10 s limit), and the
refresh of the aggregate segment takes 45 s per full recompute. Re-bench at the same size; decide
"no rollup for v1" only if both cases meet 15 s, otherwise design a rollup that also covers
property-filtered aggregates.

**C (#142). The 10 s preview timeout contradicts the 15 s aggregate target (architecture 8.2 against 19)**

`PREVIEW_TIMEOUT` is a fixed 10 s (`crates/zirv-flow-api/src/segments/mod.rs:20`), with no setting.
Meeting 15 s alone still leaves 503s for previews between 10 and 15 s. Ruling 9201125b: raise it to a
fixed 20 s, a margin above the target, with no config key, and say so in 8.2; the section 19 targets
stay.

## 5. Reproduce

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.56.0
cargo build --release -p zirv-flow-loadgen
export M4_PROJECT=zirv-flow-m4bench10m BENCH_IMAGE_TAG=0.56.0 BENCH_PG_SHM=8gb
export SEED_PEOPLE=10000000 SEED_EVENTS=50000000 SEED_LG_POOL=1000000 M4_INC_POOL=1000000
scripts/bench/m4/stack.sh up && scripts/bench/m4/setup.sh
SEED_STEPS=load scripts/bench/m4/seed.sh                    # about an hour; keep a command under the host's cap
SEED_AT=<seed_at of the load> SEED_STEPS="vacuum-people vacuum-identities" scripts/bench/m4/seed.sh
SEED_AT=<seed_at> SEED_STEPS=vacuum-events scripts/bench/m4/seed.sh
SEED_AT=<seed_at> SEED_STEPS=manifest SEED_DURATION_S=<s> scripts/bench/m4/seed.sh
for p in verify preview-jsonb index preview-slots preview-agg preview-sum refresh-trait refresh-agg incremental; do
  scripts/bench/m4/measure.sh $p; done
```

For the custom-plan set: `ALTER DATABASE zirv_flow SET plan_cache_mode = force_custom_plan`, recreate the
`zirv-flow` service so the pool reconnects, measure, then `ALTER DATABASE zirv_flow RESET plan_cache_mode`.
